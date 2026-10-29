package io.github.huntez.galdr

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import java.net.Inet4Address

/**
 * The Wi-Fi network, even when a VPN is the active (default) network: the PC talks to the phone
 * over the local Wi-Fi, not through the tunnel.
 */
object Wifi {
    @Suppress("DEPRECATION") // allNetworks: the replacement needs a callback, this is a one-off lookup
    fun network(context: Context): Network? {
        val cm = context.getSystemService(ConnectivityManager::class.java)
        return cm.allNetworks.firstOrNull { network ->
            val caps = cm.getNetworkCapabilities(network) ?: return@firstOrNull false
            caps.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) &&
                !caps.hasTransport(NetworkCapabilities.TRANSPORT_VPN)
        }
    }

    fun address(context: Context): String? {
        val cm = context.getSystemService(ConnectivityManager::class.java)
        val network = network(context) ?: return null
        return cm.getLinkProperties(network)?.linkAddresses
            ?.map { it.address }?.firstOrNull { it is Inet4Address }?.hostAddress
    }
}
