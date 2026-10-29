package io.github.huntez.galdr

import java.io.IOException

/** Microphone + Opus encoder in native code (`src/main/cpp/mic.cpp`): AAudio and libopus. */
class NativeMic(deviceId: Int, inputPreset: Int, session: Boolean, frameMs: Int, bitrate: Int) :
    AutoCloseable {
    private var handle = open(deviceId, inputPreset, session, frameMs, bitrate)

    /** Session of the stream, for Android's audio effects; only when opened with `session`. */
    val sessionId: Int get() = sessionId(handle)

    /** Samples in one frame. */
    val frameSamples: Int = frameSamples(handle)

    /** Peak of the last encoded frame, 0..1. */
    val peak: Float get() = peak(handle)

    /** Waits for the next frame and encodes it into [out] at [offset]; 0 = nothing within 100 ms. */
    @Throws(IOException::class)
    fun encode(out: ByteArray, offset: Int, gain: Float): Int = encode(handle, out, offset, gain)

    override fun close() {
        if (handle != 0L) close(handle)
        handle = 0
    }

    private companion object {
        init {
            System.loadLibrary("galdr")
        }

        @JvmStatic external fun open(deviceId: Int, inputPreset: Int, session: Boolean, frameMs: Int, bitrate: Int): Long
        @JvmStatic external fun sessionId(handle: Long): Int
        @JvmStatic external fun frameSamples(handle: Long): Int
        @JvmStatic external fun peak(handle: Long): Float
        @JvmStatic external fun encode(handle: Long, out: ByteArray, offset: Int, gain: Float): Int
        @JvmStatic external fun close(handle: Long)
    }
}
