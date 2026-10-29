package io.github.huntez.galdr

/** Constants of the Galdr protocol, version 1 (docs/PROTOCOL.md). */
object Protocol {
    val MAGIC = "GLDR".toByteArray(Charsets.US_ASCII)
    const val VERSION = 1

    /** TCP control port; discovery uses the same number over UDP. */
    const val CONTROL_PORT = 47800
    const val DEFAULT_MEDIA_PORT = 47801

    const val HELLO = 0x01
    const val WELCOME = 0x02
    const val REJECT = 0x03
    const val PING = 0x04

    const val DISCOVERY_REQUEST = 0x10
    const val DISCOVERY_REPLY = 0x11
    const val MEDIA = 0x20
    const val MEDIA_HEADER = 14

    const val CODEC_OPUS = 1
    const val SAMPLE_RATE = 48000
    const val CHANNELS = 1
    const val FRAME_MS = 20

    /** True if [data] starts with `GLDR` at [offset]. */
    fun hasMagic(data: ByteArray, offset: Int = 0, length: Int = data.size): Boolean =
        length - offset >= MAGIC.size && MAGIC.indices.all { data[offset + it] == MAGIC[it] }
}
