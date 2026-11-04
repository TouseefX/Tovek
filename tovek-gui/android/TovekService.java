package dev.tovek.gui;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;

/**
 * Termux-style keep-alive: a foreground service that runs the embedded
 * decompiler server OUTSIDE the UI process (android:process=":tovek_server"
 * in the manifest), so swiping the app away kills only the UI — the server
 * keeps listening until "Stop" is pressed in the app or on the notification.
 *
 * The service loads libtovek_gui.so itself and starts/stops the axum server
 * through the nativeStartServer/nativeStopServer JNI exports in
 * tovek-gui/src/lib.rs. It also holds a partial wake lock (CPU) and a WiFi
 * lock (LAN reachability) while running.
 *
 * This file is NOT built by xbuild: the CI workflow compiles it with
 * javac + d8 and injects the resulting classes.dex plus the <service>
 * manifest entry into the xbuild APK (see .github/workflows/gui.yaml).
 */
public final class TovekService extends Service {
    public static final String ACTION_STOP = "dev.tovek.gui.action.STOP";
    public static final String EXTRA_PORT = "port";
    public static final String EXTRA_BIND_ALL = "bind_all";
    public static final String EXTRA_CERT_PATH = "cert_path";
    public static final String EXTRA_KEY_PATH = "key_path";

    private static final String CHANNEL_ID = "tovek_server";
    private static final int NOTIFICATION_ID = 1;

    static {
        System.loadLibrary("tovek_gui");
    }

    private static native void nativeStartServer(
            int port, boolean bindAll, String certPath, String keyPath);

    private static native void nativeStopServer();

    /**
     * Clipboard helper for the UI process: eframe's clipboard backend has no
     * Android support, so the Rust side calls this instead. ClipboardManager
     * must be used from a Looper thread, hence the main-looper post.
     */
    public static void copyToClipboard(final Context context, final String text) {
        new Handler(Looper.getMainLooper()).post(new Runnable() {
            @Override
            public void run() {
                ClipboardManager cm =
                        (ClipboardManager) context.getSystemService(Context.CLIPBOARD_SERVICE);
                if (cm != null) {
                    cm.setPrimaryClip(ClipData.newPlainText("Tovek", text));
                }
            }
        });
    }

    private String lastNotificationText;
    private PowerManager.WakeLock wakeLock;
    private WifiManager.WifiLock wifiLock;

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP.equals(intent.getAction())) {
            shutdown();
            return START_NOT_STICKY;
        }

        // START_STICKY restarts redeliver a *null* intent. Persist the last
        // explicit configuration so a system restart brings the server back
        // exactly as it was (same port AND same TLS mode — an http server on
        // an https URL leaves executors hanging forever).
        SharedPreferences prefs = getSharedPreferences("tovek_service", MODE_PRIVATE);
        int port;
        boolean bindAll;
        String certPath;
        String keyPath;
        if (intent != null) {
            port = intent.getIntExtra(EXTRA_PORT, 3000);
            bindAll = intent.getBooleanExtra(EXTRA_BIND_ALL, true);
            certPath = intent.getStringExtra(EXTRA_CERT_PATH);
            keyPath = intent.getStringExtra(EXTRA_KEY_PATH);
            prefs.edit()
                    .putInt("port", port)
                    .putBoolean("bind_all", bindAll)
                    .putString("cert", certPath)
                    .putString("key", keyPath)
                    .apply();
        } else {
            port = prefs.getInt("port", 3000);
            bindAll = prefs.getBoolean("bind_all", true);
            certPath = prefs.getString("cert", null);
            keyPath = prefs.getString("key", null);
        }
        boolean https = certPath != null && keyPath != null;

        lastNotificationText =
                "Server listening on port " + port + (https ? " (https)" : " (http)");
        startForegroundWithNotification(lastNotificationText);
        acquireLocks();
        nativeStartServer(port, bindAll, certPath, keyPath);
        return START_STICKY;
    }

    @Override
    public void onTaskRemoved(Intent rootIntent) {
        // Surviving the task swipe is the whole point. Re-assert the
        // foreground notification so the OS keeps treating us as foreground.
        if (lastNotificationText != null) {
            startForegroundWithNotification(lastNotificationText);
        }
    }

    @Override
    public void onDestroy() {
        nativeStopServer();
        releaseLocks();
        super.onDestroy();
    }

    private void startForegroundWithNotification(String text) {
        NotificationManager nm = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        NotificationChannel channel =
                new NotificationChannel(CHANNEL_ID, "Tovek server", NotificationManager.IMPORTANCE_LOW);
        channel.setDescription("Keeps the decompiler server running in the background");
        nm.createNotificationChannel(channel);

        Notification.Builder builder = new Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Tovek")
                .setContentText(text)
                .setSmallIcon(Icon.createWithResource(this, android.R.drawable.stat_notify_sync_noanim))
                .setOngoing(true);

        Intent open = getPackageManager().getLaunchIntentForPackage(getPackageName());
        if (open != null) {
            builder.setContentIntent(PendingIntent.getActivity(
                    this, 0, open, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT));
        }

        Intent stop = new Intent(this, TovekService.class).setAction(ACTION_STOP);
        PendingIntent stopPi = PendingIntent.getService(
                this, 1, stop, PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        builder.addAction(new Notification.Action.Builder(
                Icon.createWithResource(this, android.R.drawable.ic_delete), "Stop", stopPi).build());

        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, builder.build(),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC);
        } else {
            startForeground(NOTIFICATION_ID, builder.build());
        }
    }

    private void acquireLocks() {
        if (wakeLock == null) {
            PowerManager pm = (PowerManager) getSystemService(Context.POWER_SERVICE);
            wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "tovek:server");
            wakeLock.setReferenceCounted(false);
        }
        if (!wakeLock.isHeld()) {
            wakeLock.acquire();
        }
        if (wifiLock == null) {
            WifiManager wm = (WifiManager) getApplicationContext().getSystemService(Context.WIFI_SERVICE);
            if (wm != null) {
                wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, "tovek:server");
                wifiLock.setReferenceCounted(false);
            }
        }
        if (wifiLock != null && !wifiLock.isHeld()) {
            wifiLock.acquire();
        }
    }

    private void releaseLocks() {
        if (wakeLock != null && wakeLock.isHeld()) {
            wakeLock.release();
        }
        if (wifiLock != null && wifiLock.isHeld()) {
            wifiLock.release();
        }
    }

    private void shutdown() {
        nativeStopServer();
        releaseLocks();
        stopForeground(STOP_FOREGROUND_REMOVE);
        stopSelf();
    }
}
