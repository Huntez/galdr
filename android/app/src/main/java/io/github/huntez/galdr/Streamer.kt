package io.github.huntez.galdr

import android.annotation.SuppressLint
import android.content.Context
import android.media.AudioFormat
import android.media.AudioManager
import android.media.AudioRecord
import android.media.MediaCodec
import android.media.MediaFormat
import android.media.MediaRecorder
import android.media.audiofx.AudioEffect
import android.media.audiofx.AutomaticGainControl
import android.media.audiofx.NoiseSuppressor
import android.os.Process
import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import java.nio.ByteOrder
import kotlin.concurrent.thread
import kotlin.math.abs
import kotlin.math.max

/** Captures the microphone, encodes Opus and sends media packets to the PC until closed. */
class Streamer(
    private val context: Context,
    private val target: InetSocketAddress,
    private val processed: () -> Boolean,
    private val micAddress: () -> String,
    private val bitrate: Int,
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
    @SuppressLint("MissingPermission") // granted before the service starts
    private inner class Mic(val processed: Boolean, val address: String) : AutoCloseable {
        val record: AudioRecord
        private val effects = mutableListOf<AudioEffect>()

        init {
            val source = when {
                processed -> MediaRecorder.AudioSource.VOICE_COMMUNICATION
                unprocessedSupported -> MediaRecorder.AudioSource.UNPROCESSED
                else -> MediaRecorder.AudioSource.VOICE_RECOGNITION
            }
            val minBuffer = AudioRecord.getMinBufferSize(
                SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT,
            )
            record = AudioRecord.Builder()
                .setAudioSource(source)
                .setAudioFormat(
                    AudioFormat.Builder()
                        .setSampleRate(SAMPLE_RATE)
                        .setChannelMask(AudioFormat.CHANNEL_IN_MONO)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .build(),
                )
                .setBufferSizeInBytes(max(minBuffer, FRAME * 2 * 2))
                .build()
            Mics.find(context, address)?.let(record::setPreferredDevice)
            if (processed) {
                val session = record.audioSessionId
                if (NoiseSuppressor.isAvailable()) NoiseSuppressor.create(session)?.let(effects::add)
                if (AutomaticGainControl.isAvailable()) AutomaticGainControl.create(session)?.let(effects::add)
                effects.forEach { it.enabled = true }
            }
            record.startRecording()
            Log.i(TAG, "microphone: '$address', source $source, effects ${effects.map { it.descriptor.name }}")
        }

        override fun close() {
            effects.forEach { it.release() }
            runCatching { record.stop() }
            record.release()
        }
    }

    private fun run() {
        var mic = Mic(processed(), micAddress())
        val codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
        val socket = DatagramSocket()
        // Prefer Wi-Fi when a VPN is the default network. A VPN may forbid this; then the
        // system routing is used, which reaches the LAN when the VPN excludes it.
        runCatching { Wifi.network(context)?.bindSocket(socket) }
            .onFailure { Log.w(TAG, "could not bind to Wi-Fi: $it") }
        // DSCP EF: WMM routers put these into the Wi-Fi voice queue (less jitter under load).
        runCatching { socket.trafficClass = DSCP_VOICE }
        try {
            codec.configure(
                MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, SAMPLE_RATE, 1).apply {
                    setInteger(MediaFormat.KEY_BIT_RATE, bitrate)
                    setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, FRAME * 2)
                },
                null, null, MediaCodec.CONFIGURE_FLAG_ENCODE,
            )
            codec.start()
            Log.i(TAG, "streaming to $target, ${bitrate / 1000} kbit/s")

            val pcm = ShortArray(FRAME)
            val info = MediaCodec.BufferInfo()
            val packet = ByteArray(HEADER + MAX_OPUS)
            val datagram = DatagramPacket(packet, packet.size, target)
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
                val read = mic.record.read(pcm, 0, FRAME)
                if (read <= 0) continue
                val gain = gain()
                var peak = 0
                for (i in 0 until read) {
                    val v = if (gain == 1f) pcm[i].toInt() else (pcm[i] * gain).toInt().coerceIn(-32768, 32767)
                    pcm[i] = v.toShort()
                    peak = max(peak, abs(v))
                }
                Status.level = peak / 32768f

                val input = codec.dequeueInputBuffer(CODEC_WAIT_US)
                // The encoder runs on its own thread: wait briefly for this frame's packet,
                // otherwise it would only go out after the next 20 ms read.
                var wait = 0L
                if (input >= 0) {
                    codec.getInputBuffer(input)!!.apply { clear() }
                        .order(ByteOrder.nativeOrder()).asShortBuffer().put(pcm, 0, read)
                    codec.queueInputBuffer(input, 0, read * 2, samples * 1_000_000 / SAMPLE_RATE, 0)
                    samples += read
                    wait = OUTPUT_WAIT_US
                }

                while (true) {
                    val output = codec.dequeueOutputBuffer(info, wait)
                    if (output == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) continue
                    if (output < 0) break
                    wait = 0
                    val config = info.flags and MediaCodec.BUFFER_FLAG_CODEC_CONFIG != 0
                    if (!config && info.size in 1..MAX_OPUS) {
                        codec.getOutputBuffer(output)!!.get(packet, HEADER, info.size)
                        writeHeader(packet, seq, info.presentationTimeUs * SAMPLE_RATE / 1_000_000)
                        datagram.length = HEADER + info.size
                        socket.send(datagram)
                        seq = (seq + 1) and 0xffff_ffffL
                    }
                    codec.releaseOutputBuffer(output, false)
                }
            }
        } finally {
            Status.level = 0f
            mic.close()
            runCatching { codec.stop() }
            codec.release()
            socket.close()
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
        private const val SAMPLE_RATE = Protocol.SAMPLE_RATE
        private const val FRAME = SAMPLE_RATE * Protocol.FRAME_MS / 1000
        private const val HEADER = Protocol.MEDIA_HEADER
        private const val MAX_OPUS = 1500
        private const val CODEC_WAIT_US = 10_000L
        /** Encoding 20 ms of audio takes about a millisecond; this only caps a slow phone. */
        private const val OUTPUT_WAIT_US = 5_000L
        private const val DSCP_VOICE = 0xB8
        private const val TAG = "galdr"
    }
}
