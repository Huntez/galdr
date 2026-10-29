package io.github.huntez.galdr

import android.content.Context

/** Persistent user choices. Read when a PC connects. */
class Settings(context: Context) {
    private val prefs = context.getSharedPreferences("settings", Context.MODE_PRIVATE)

    /** Android noise suppression / gain control instead of the raw microphone. */
    var processed: Boolean
        get() = prefs.getBoolean("processed", false)
        set(value) = prefs.edit().putBoolean("processed", value).apply()

    /** Software gain applied before encoding, 1 = unchanged. Read live by the streamer. */
    var gain: Float
        get() = prefs.getFloat("gain", 1f)
        set(value) = prefs.edit().putFloat("gain", value).apply()

    var bitrate: Int
        get() = prefs.getInt("bitrate", 128_000)
        set(value) = prefs.edit().putInt("bitrate", value).apply()

    /** TCP port the phone listens on; the PC must use the same. Applies on the next Start. */
    var port: Int
        // 8125 was an earlier default.
        get() = prefs.getInt("port", Protocol.CONTROL_PORT).let { if (it == 8125) Protocol.CONTROL_PORT else it }
        set(value) = prefs.edit().putInt("port", value).apply()

    /** Address of the chosen built-in microphone, "" = Android chooses. Read live. */
    var mic: String
        get() = prefs.getString("mic", "")!!
        set(value) = prefs.edit().putString("mic", value).apply()

    var autoStart: Boolean
        get() = prefs.getBoolean("autoStart", false)
        set(value) = prefs.edit().putBoolean("autoStart", value).apply()

    companion object {
        val BITRATES = listOf(64_000, 96_000, 128_000, 192_000, 256_000)
        const val MAX_GAIN = 4f
    }
}
