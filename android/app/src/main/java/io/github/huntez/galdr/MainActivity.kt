package io.github.huntez.galdr

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameMillis
import kotlinx.coroutines.delay

class MainActivity : ComponentActivity() {
    /** Bumped on every return to the app: settings such as USB debugging may have changed. */
    private var resumes by mutableIntStateOf(0)

    override fun onResume() {
        super.onResume()
        resumes++
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        enableEdgeToEdge()
        setContent { GaldrTheme { App() } }
    }

    @Composable
    private fun App() {
        val settings = remember { Settings(this) }
        var processed by remember { mutableStateOf(settings.processed) }
        var bitrate by remember { mutableIntStateOf(settings.bitrate) }
        var frameMs by remember { mutableIntStateOf(settings.frameMs) }
        var gain by remember { mutableFloatStateOf(settings.gain) }
        var port by remember { mutableIntStateOf(settings.port) }
        var mic by remember { mutableStateOf(settings.mic) }
        var autoStart by remember { mutableStateOf(settings.autoStart) }
        val mics = remember { Mics.list(this) }
        val name = remember { Device.name(this) }
        var denied by remember { mutableStateOf(false) }
        var version by remember { mutableIntStateOf(0) }
        var level by remember { mutableFloatStateOf(0f) }

        DisposableEffect(Unit) {
            val listener: () -> Unit = { version++ }
            Status.listen(listener)
            onDispose { Status.unlisten(listener) }
        }
        LaunchedEffect(Unit) {
            if (settings.autoStart && Status.phase == Status.Phase.STOPPED && hasMicrophone()) startService()
        }
        val streaming = version.let { Status.phase == Status.Phase.STREAMING }
        LaunchedEffect(streaming) {
            level = 0f
            // ~20 updates/s; withFrameMillis suspends while nothing is drawn (app in background,
            // screen off), so the loop doesn't wake the phone for an invisible meter.
            while (streaming) {
                delay(50)
                withFrameMillis {}
                level = Status.level
            }
        }

        val permissions = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestMultiplePermissions(),
        ) {
            denied = !hasMicrophone()
            if (!denied) startService()
        }

        version // re-read Status below whenever the service reports a change
        // A system call: only when the status changes, not on every recomposition.
        val address = remember(version) { Wifi.address(this) }
        val usbDebugging = remember(version, resumes) { Device.usbDebugging(this) }
        Screen(
            ScreenState(
                phase = Status.phase,
                name = name,
                peer = Status.peer,
                error = Status.error,
                address = address,
                overUsb = Status.usb,
                usbDebugging = usbDebugging,
                level = { level },
                processed = processed,
                bitrate = bitrate,
                frameMs = frameMs,
                gain = gain,
                port = port,
                mics = mics,
                mic = mic.takeIf { saved -> mics.any { it.address == saved } } ?: "",
                autoStart = autoStart,
                permissionDenied = denied,
            ),
            object : ScreenActions {
                override fun toggle() {
                    if (Status.phase != Status.Phase.STOPPED) {
                        stopService(Intent(this@MainActivity, MicService::class.java))
                        return
                    }
                    val missing = buildList {
                        add(Manifest.permission.RECORD_AUDIO)
                        if (Build.VERSION.SDK_INT >= 33) add(Manifest.permission.POST_NOTIFICATIONS)
                    }.filter { checkSelfPermission(it) != PackageManager.PERMISSION_GRANTED }
                    if (missing.isEmpty()) startService() else permissions.launch(missing.toTypedArray())
                }

                override fun processed(on: Boolean) {
                    processed = on
                    settings.processed = on
                }

                override fun bitrate(rate: Int) {
                    bitrate = rate
                    settings.bitrate = rate
                }

                override fun frameMs(ms: Int) {
                    frameMs = ms
                    settings.frameMs = ms
                }

                override fun gain(value: Float) {
                    gain = value
                    settings.gain = value
                }

                override fun port(port: Int) {
                    setPort(port)
                }

                override fun mic(address: String) {
                    mic = address
                    settings.mic = address
                }

                override fun autoStart(on: Boolean) {
                    autoStart = on
                    settings.autoStart = on
                }

                private fun setPort(value: Int) {
                    port = value
                    settings.port = value
                }
            },
        )
    }

    private fun hasMicrophone() =
        checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED

    private fun startService() {
        startForegroundService(Intent(this, MicService::class.java))
    }
}
