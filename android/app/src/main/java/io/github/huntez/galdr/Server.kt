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
    private val openStream: (InetSocketAddress) -> AutoCloseable,
    /** The PC's name and address while one is connected, null when it left. */
    private val onPeer: (String?) -> Unit,
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
    private fun startStream(target: InetSocketAddress): AutoCloseable = synchronized(streamLock) {
        stream?.close()
        openStream(target).also { stream = it }
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
            fun send(type: Int, payload: ByteArray = ByteArray(0)) = synchronized(output) {
                output.writeByte(type)
                output.writeShort(payload.size)
                output.write(payload)
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

            send(Protocol.WELCOME, welcome())
            stream = startStream(InetSocketAddress(peer, mediaPort))
            onPeer("$pcName (${peer.hostAddress})")
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
                    if (!listener.isClosed) onPeer(null)
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

    private fun welcome(): ByteArray = ByteArrayOutputStream().apply {
        val out = DataOutputStream(this)
        out.write(Protocol.MAGIC)
        out.writeByte(Protocol.VERSION)
        out.writeByte(Protocol.CODEC_OPUS)
        out.writeInt(Protocol.SAMPLE_RATE)
        out.writeByte(Protocol.CHANNELS)
        out.writeByte(Protocol.FRAME_MS)
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
