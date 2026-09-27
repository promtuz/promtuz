package com.promtuz.core

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.os.Build
import android.os.Handler
import android.os.Looper
import timber.log.Timber

/** One process-lifetime watch, matching core's lifetime rather than an activity's. */
internal object CoreNetworkMonitor {
    private val handler = Handler(Looper.getMainLooper())
    private var callback: ConnectivityManager.NetworkCallback? = null

    @Synchronized
    fun start(context: Context) {
        if (callback != null) return
        val manager = context.getSystemService(ConnectivityManager::class.java) ?: return
        val watch = object : ConnectivityManager.NetworkCallback() {
            private var currentNetwork: Network? = null
            private var currentRoute: Route? = null
            private var deliveredRoute: Route? = null
            private val deliver = Runnable {
                val route = currentRoute
                if (route != null && route != deliveredRoute) {
                    deliveredRoute = route
                    CoreBridge.onNetworkChanged()
                }
            }

            override fun onAvailable(network: Network) {
                if (currentNetwork == network) return
                currentNetwork = network
                currentRoute = null
                deliveredRoute = null
                handler.removeCallbacks(deliver)
                // On API 26+, onLinkPropertiesChanged follows onAvailable.
                // Use those properties; synchronous queries here can be stale.
            }

            override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
                if (network != currentNetwork) return
                val route = Route(
                    network.networkHandle,
                    properties.interfaceName,
                    properties.linkAddresses.map { it.toString() }.toSet(),
                    properties.routes.map { it.toString() }.toSet(),
                    properties.mtu,
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) properties.nat64Prefix?.toString() else null,
                )
                if (route == currentRoute) return
                currentRoute = route
                handler.removeCallbacks(deliver)
                // Wi-Fi/cellular handovers often emit several address/route updates.
                handler.postDelayed(deliver, 400)
            }

            override fun onLost(network: Network) {
                // A delayed loss for the previous network must not erase its replacement.
                if (network != currentNetwork) return
                currentNetwork = null
                currentRoute = null
                deliveredRoute = null
                handler.removeCallbacks(deliver)
                // Retry once a network is usable, including a return to the same route.
            }
        }
        runCatching { manager.registerDefaultNetworkCallback(watch, handler) }
            .onSuccess { callback = watch }
            .onFailure { Timber.tag("CoreNetwork").w(it, "network watch failed") }
    }

    // Deliberately omit capability flags, DNS and lease lifetimes: their frequent
    // updates do not invalidate an established UDP path. Never log IP addresses.
    private data class Route(
        val networkHandle: Long,
        val interfaceName: String?,
        val addresses: Set<String>,
        val routes: Set<String>,
        val mtu: Int,
        val nat64Prefix: String?,
    )
}
