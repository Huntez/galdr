package io.github.huntez.galdr

import android.content.Context
import android.os.Build

/** How this phone introduces itself to PCs, and on its own screen. */
object Device {
    /** The name set in Android's settings (About phone), else the model. */
    fun name(context: Context): String =
        android.provider.Settings.Global.getString(context.contentResolver, android.provider.Settings.Global.DEVICE_NAME)
            ?: Build.MODEL

    /** USB debugging is on, so a PC can reach the phone over a cable through adb. */
    fun usbDebugging(context: Context): Boolean =
        android.provider.Settings.Global.getInt(context.contentResolver, android.provider.Settings.Global.ADB_ENABLED, 0) == 1

    /** Shown on the PC under the name: the model when the phone was renamed, and Android. */
    fun details(name: String): String {
        val android = "Android ${Build.VERSION.RELEASE}"
        return if (name == Build.MODEL) android else "${Build.MODEL} · $android"
    }
}
