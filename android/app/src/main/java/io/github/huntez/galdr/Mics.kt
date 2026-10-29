package io.github.huntez.galdr

import android.content.Context
import android.media.AudioDeviceInfo
import android.media.AudioManager

/** A microphone the user can pick. [address] "" means Android chooses. */
data class MicChoice(val address: String, val label: String)

/** The phone's built-in microphones (Pixel: "bottom", "back", ...). */
object Mics {
    fun list(context: Context): List<MicChoice> =
        listOf(MicChoice("", "Auto")) + builtIn(context).mapIndexed { i, device ->
            MicChoice(device.address, device.address.ifBlank { "Mic ${i + 1}" }.replaceFirstChar(Char::uppercase))
        }.distinctBy { it.address }

    fun find(context: Context, address: String): AudioDeviceInfo? =
        if (address.isEmpty()) null else builtIn(context).firstOrNull { it.address == address }

    private fun builtIn(context: Context) = context.getSystemService(AudioManager::class.java)
        .getDevices(AudioManager.GET_DEVICES_INPUTS)
        .filter { it.type == AudioDeviceInfo.TYPE_BUILTIN_MIC }
}
