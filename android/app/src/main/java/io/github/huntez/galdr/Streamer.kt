package io.github.huntez.galdr

import android.content.Context
import android.media.AudioManager
import android.media.MediaRecorder
import android.media.audiofx.AudioEffect
import android.media.audiofx.AutomaticGainControl
import android.media.audiofx.NoiseSuppressor
import android.os.Process
import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import kotlin.concurrent.thread

/** Where media packets go. */
sealed interface MediaLink {
    /** Datagrams to the PC's media port (Wi-Fi). */
    data class Udp(val target: InetSocketAddress) : MediaLink

    /** MEDIA messages on the control connection (USB, through adb). */
    class InBand(val send: (packet: ByteArray, length: Int) -> Unit) : MediaLink
}

/** Captures the microphone, encodes Opus and sends media packets to the PC until closed. */
class Streamer(
    private val context: Context,
    private val link: MediaLink,
    private val processed: () -> Boolean,
    private val micAddress: () -> String,
    private val bitrate: Int,
    /** Frame length agreed in WELCOME. */
    private val frameMs: Int,
    private val gain: () -> Float,
) : AutoCloseable {
    private val unprocessedSupported = context.getSystemService(AudioManager::class.java)
        .getProperty(AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED) == "true"

    @Volatile private var running = true
    private val worker = thread(name = "galdr-audio") {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)
        try {
            run()
        } catch (e: Exception) {
            Log.e(TAG, "audio stopped", e)
            if (running) Status.fail("Audio: ${e.message ?: e.javaClass.simpleName}")
        }
    }

    /**
     * The microphone in one mode. Processed: the voice-call source with Android's noise
     * suppressor and gain control switched on explicitly (the source alone doesn't enable them
     * on every phone). Raw: the unprocessed source.
     */
    private inner class Mic(val processed: Boolean, val address: String) : AutoCloseable {
        // AAudio's input presets use the same numbers as these sources.
        private val source = when {
            processed -> MediaRecorder.AudioSource.VOICE_COMMUNICATION
            unprocessedSupported -> MediaRecorder.AudioSource.UNPROCESSED
            else -> MediaRecorder.AudioSource.VOICE_RECOGNITION
        }
        val native = NativeMic(Mics.find(context, address)?.id ?: 0, source, processed, frameMs, bitrate)
        private val effects = mutableListOf<AudioEffect>()

        init {
            if (processed) {
                val session = native.sessionId
                if (NoiseSuppressor.isAvailable()) NoiseSuppressor.create(session)?.let(effects::add)
                if (AutomaticGainControl.isAvailable()) AutomaticGainControl.create(session)?.let(effects::add)
                effects.forEach { it.enabled = true }
            }
            Log.i(TAG, "microphone: '$address', source $source, effects ${effects.map { it.descriptor.name }}")
        }

        override fun close() {
            effects.forEach { it.release() }
            native.close()
        }
    }

    private fun run() {
        var mic = Mic(processed(), micAddress())
        val packet = ByteArray(HEADER + MAX_OPUS)
        val socket = (link as? MediaLink.Udp)?.let { DatagramSocket() }
        val send: (Int) -> Unit = when (link) {
            is MediaLink.Udp -> {
                // Prefer Wi-Fi when a VPN is the default network. A VPN may forbid this; then the
                // system routing is used, which reaches the LAN when the VPN excludes it.
                runCatching { Wifi.network(context)?.bindSocket(socket!!) }
                    .onFailure { Log.w(TAG, "could not bind to Wi-Fi: $it") }
                // DSCP EF: WMM routers put these into the Wi-Fi voice queue (less jitter under load).
                runCatching { socket!!.trafficClass = DSCP_VOICE }
                val datagram = DatagramPacket(packet, packet.size, link.target)
                fun(length: Int) {
                    datagram.length = length
                    socket!!.send(datagram)
                }
            }
            is MediaLink.InBand -> { length -> link.send(packet, length) }
        }
        try {
            val to = if (link is MediaLink.Udp) "${link.target}" else "USB"
            Log.i(TAG, "streaming to $to, ${bitrate / 1000} kbit/s, $frameMs ms frames")
            var samples = 0L
            var seq = 0L
            while (running) {
                // Both choices apply live: reopen the microphone when one changes.
                val processed = processed()
                val address = micAddress()
                if (processed != mic.processed || address != mic.address) {
                    mic.close()
                    mic = Mic(processed, address)
                }
                val size = mic.native.encode(packet, HEADER, gain())
                if (size <= 0) continue
                Status.level = mic.native.peak
                writeHeader(packet, seq, samples)
                send(HEADER + size)
                samples += mic.native.frameSamples
                seq = (seq + 1) and 0xffff_ffffL
            }
        } finally {
            Status.level = 0f
            mic.close()
            socket?.close()
        }
    }

    /** `GLDR | 0x20 | flags | seq:u32 | timestamp:u32` (in samples), see docs/PROTOCOL.md. */
    private fun writeHeader(packet: ByteArray, seq: Long, timestamp: Long) {
        Protocol.MAGIC.copyInto(packet)
        packet[4] = Protocol.MEDIA.toByte()
        packet[5] = 0
        for (i in 0 until 4) {
            packet[6 + i] = (seq shr (24 - 8 * i)).toByte()
            packet[10 + i] = (timestamp shr (24 - 8 * i)).toByte()
        }
    }

    override fun close() {
        running = false
        worker.join(1000)
    }

    companion object {
        private const val HEADER = Protocol.MEDIA_HEADER
        private const val MAX_OPUS = 1500
        private const val DSCP_VOICE = 0xB8
        private const val TAG = "galdr"
    }
}
