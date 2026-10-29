package io.github.huntez.galdr

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.util.Log
import java.io.IOException

/** Foreground service that keeps the control server and microphone running in the background. */
class MicService : Service() {
    private var server: Server? = null
    private var discovery: Discovery? = null
    private var wifiLock: WifiManager.WifiLock? = null
    private var multicastLock: WifiManager.MulticastLock? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_STOP) {
            stopSelf()
            return START_NOT_STICKY
        }
        if (server != null) return START_NOT_STICKY

        startForeground(NOTIFICATION_ID, notification(), ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        val settings = Settings(this)
        val name = android.provider.Settings.Global.getString(contentResolver, android.provider.Settings.Global.DEVICE_NAME)
            ?: Build.MODEL
        server = try {
            Server(
                settings.port,
                name,
                openStream = { target ->
                    Streamer(this, target, { settings.processed }, { settings.mic }, settings.bitrate) { settings.gain }
                },
                onPeer = { peer ->
                    Status.set(if (peer == null) Status.Phase.WAITING else Status.Phase.STREAMING, peer)
                },
            )
        } catch (e: IOException) {
            Status.set(Status.Phase.STOPPED, error = "Port ${settings.port}: ${e.message}")
            stopSelf()
            return START_NOT_STICKY
        }
        // Without discovery the PC can still connect by IP.
        // Shown on the PC under the name: the model when the user renamed the phone, and Android.
        val android = "Android ${Build.VERSION.RELEASE}"
        val details = if (name == Build.MODEL) android else "${Build.MODEL} · $android"
        discovery = runCatching { Discovery(this, settings.port, name, details) }
            .onFailure { Log.w(TAG, "discovery: $it") }
            .getOrNull()
        val wifi = getSystemService(WifiManager::class.java)
        wifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "galdr").apply { acquire() }
        multicastLock = wifi.createMulticastLock("galdr").apply { acquire() }
        Status.set(Status.Phase.WAITING)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        discovery?.close()
        discovery = null
        server?.close()
        server = null
        wifiLock?.release()
        multicastLock?.release()
        if (Status.error == null) Status.set(Status.Phase.STOPPED)
    }

    private fun notification(): Notification {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, "Microphone server", NotificationManager.IMPORTANCE_LOW),
        )
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val stop = PendingIntent.getService(
            this, 1, Intent(this, MicService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL)
            .setSmallIcon(android.R.drawable.ic_btn_speak_now)
            .setContentTitle("Galdr")
            .setContentText("Microphone server is running")
            .setContentIntent(open)
            .addAction(Notification.Action.Builder(null, "Stop", stop).build())
            .setOngoing(true)
            .build()
    }

    companion object {
        private const val ACTION_STOP = "io.github.huntez.galdr.STOP"
        private const val CHANNEL = "server"
        private const val NOTIFICATION_ID = 1
        private const val TAG = "galdr"
    }
}
