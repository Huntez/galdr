package io.github.huntez.galdr

import android.util.Log
import java.io.ByteArrayOutputStream
import java.io.DataInputStream
import java.io.DataOutputStream
import java.io.IOException
import java.net.InetSocketAddress
import java.net.ServerSocket
import java.net.Socket
import kotlin.concurrent.thread

/**
 * Control channel (docs/PROTOCOL.md): listens on TCP, answers `HELLO` with `WELCOME` and streams
 * to the PC. One PC at a time; a new connection replaces the old one.
 */
class Server(
    port: Int,
    private val phoneName: String,
    /** Opus frame length for the next session, read when a PC connects. */
    private val frameMs: () -> Int,
    /** Starts streaming to the PC with the given frame length. */
    private val openStream: (MediaLink, Int) -> AutoCloseable,
    /** The PC's name and address while one is connected (null when it left), and if over USB. */
    private val onPeer: (peer: String?, usb: Boolean) -> Unit,
) : AutoCloseable {
    private val listener = ServerSocket().apply {
        reuseAddress = true
        bind(InetSocketAddress(port))
    }
    private var session: Socket? = null
    private val streamLock = Any()
    private var stream: AutoCloseable? = null

    init {
        thread(name = "galdr-accept") { acceptLoop() }
    }

    private fun acceptLoop() {
        while (!listener.isClosed) {
            val socket = try {
                listener.accept()
            } catch (e: IOException) {
                break
            }
            synchronized(this) {
                session?.close()
                session = socket
            }
            thread(name = "galdr-control") { serve(socket) }
        }
    }

    /**
     * The previous PC's session ends on its own thread; close its stream here first, so two
     * streams never hold the microphone at once.
     */
    private fun startStream(link: MediaLink, frameMs: Int): AutoCloseable = synchronized(streamLock) {
        stream?.close()
        openStream(link, frameMs).also { stream = it }
    }

    private fun stopStream(mine: AutoCloseable) = synchronized(streamLock) {
        mine.close()
        if (stream === mine) stream = null
    }

    private fun serve(socket: Socket) {
        val peer = socket.inetAddress
        var stream: AutoCloseable? = null
        var pinger: Thread? = null
        try {
            socket.tcpNoDelay = true
            socket.soTimeout = IDLE_TIMEOUT_MS
            val input = DataInputStream(socket.getInputStream().buffered())
            val output = DataOutputStream(socket.getOutputStream().buffered())
            fun send(type: Int, payload: ByteArray = ByteArray(0), length: Int = payload.size) =
                synchronized(output) {
                    output.writeByte(type)
                    output.writeShort(length)
                    output.write(payload, 0, length)
                    output.flush()
                }

            val (type, hello) = read(input)
            if (type != Protocol.HELLO || !Protocol.hasMagic(hello) || hello.size < 7) {
                throw IOException("expected HELLO")
            }
            if (hello[4].toInt() != Protocol.VERSION) {
                send(Protocol.REJECT, "This phone speaks Galdr protocol ${Protocol.VERSION}; update Galdr".toByteArray())
                return
            }
            val mediaPort = ((hello[5].toInt() and 0xff) shl 8) or (hello[6].toInt() and 0xff)
            val pcName = String(hello, 7, hello.size - 7, Charsets.UTF_8).ifBlank { "PC" }

            val frameMs = frameMs()
            send(Protocol.WELCOME, welcome(frameMs))
            // Port 0: adb forwards only this TCP connection (USB), so the audio comes along.
            val link = if (mediaPort == 0) {
                MediaLink.InBand { packet, length ->
                    // A failed write ends the session through the read loop below.
                    try {
                        send(Protocol.MEDIA_INBAND, packet, length)
                    } catch (_: IOException) {
                    }
                }
            } else {
                MediaLink.Udp(InetSocketAddress(peer, mediaPort))
            }
            stream = startStream(link, frameMs)
            onPeer(if (mediaPort == 0) "$pcName (USB)" else "$pcName (${peer.hostAddress})", mediaPort == 0)
            pinger = thread(name = "galdr-ping") {
                try {
                    while (!socket.isClosed) {
                        send(Protocol.PING)
                        Thread.sleep(PING_PERIOD_MS)
                    }
                } catch (_: Exception) {
                    // The socket closed; serve() cleans up.
                }
            }
            while (true) read(input) // PINGs; a timeout or a closed connection ends the loop
        } catch (e: IOException) {
            Log.i(TAG, "session ${peer.hostAddress} ended: ${e.message}")
        } finally {
            stream?.let(::stopStream)
            socket.close()
            pinger?.interrupt()
            synchronized(this) {
                if (session === socket) {
                    session = null
                    if (!listener.isClosed) onPeer(null, false)
                }
            }
        }
    }

    private fun read(input: DataInputStream): Pair<Int, ByteArray> {
        val type = input.readUnsignedByte()
        val len = input.readUnsignedShort()
        if (len > MAX_PAYLOAD) throw IOException("message too long: $len")
        return type to ByteArray(len).also { input.readFully(it) }
    }

    private fun welcome(frameMs: Int): ByteArray = ByteArrayOutputStream().apply {
        val out = DataOutputStream(this)
        out.write(Protocol.MAGIC)
        out.writeByte(Protocol.VERSION)
        out.writeByte(Protocol.CODEC_OPUS)
        out.writeInt(Protocol.SAMPLE_RATE)
        out.writeByte(Protocol.CHANNELS)
        out.writeByte(frameMs)
        out.write(phoneName.toByteArray(Charsets.UTF_8))
    }.toByteArray()

    override fun close() {
        listener.close()
        synchronized(this) { session?.close() }
    }

    companion object {
        private const val MAX_PAYLOAD = 4096
        private const val PING_PERIOD_MS = 1000L
        /** The PC pings every second. */
        private const val IDLE_TIMEOUT_MS = 5000
        private const val TAG = "galdr"
    }
}
