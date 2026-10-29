package io.github.huntez.galdr

import android.content.Context
import android.util.Log
import java.io.ByteArrayOutputStream
import java.io.DataOutputStream
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress
import kotlin.concurrent.thread

/** Answers discovery requests from PCs (docs/PROTOCOL.md, "Discovery") until closed. */
class Discovery(context: Context, controlPort: Int, name: String, details: String) : AutoCloseable {
    private val socket = DatagramSocket(null).apply {
        reuseAddress = true
        bind(InetSocketAddress(controlPort))
        // Answer over Wi-Fi even when a VPN is the default network; a VPN may forbid it.
        runCatching { Wifi.network(context)?.bindSocket(this) }
    }

    private val reply: ByteArray = ByteArrayOutputStream().apply {
        val out = DataOutputStream(this)
        out.write(Protocol.MAGIC)
        out.writeByte(Protocol.DISCOVERY_REPLY)
        out.writeByte(Protocol.VERSION)
        out.writeShort(controlPort)
        val nameBytes = name.toByteArray(Charsets.UTF_8).take(255).toByteArray()
        out.writeByte(nameBytes.size)
        out.write(nameBytes)
        out.write(details.toByteArray(Charsets.UTF_8))
    }.toByteArray()

    init {
        thread(name = "galdr-discovery") {
            val buf = ByteArray(64)
            val request = DatagramPacket(buf, buf.size)
            while (!socket.isClosed) {
                try {
                    request.length = buf.size
                    socket.receive(request)
                    val ok = request.length >= 6 && Protocol.hasMagic(buf, 0, request.length) &&
                        buf[4].toInt() == Protocol.DISCOVERY_REQUEST && buf[5].toInt() == Protocol.VERSION
                    if (ok) socket.send(DatagramPacket(reply, reply.size, request.socketAddress))
                } catch (e: Exception) {
                    if (!socket.isClosed) Log.w(TAG, "discovery: $e")
                }
            }
        }
    }

    override fun close() = socket.close()

    companion object {
        private const val TAG = "galdr"
    }
}
