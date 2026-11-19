package com.promtuz.chat.security

import android.view.Window
import android.view.WindowManager
import java.util.WeakHashMap

/** Main-thread leases also cover overlapping screens during navigation animations. */
internal object SecureRecoveryWindow {
    private class Lease(var count: Int, val previouslySecure: Boolean)
    private val leases = WeakHashMap<Window, Lease>()

    fun acquire(window: Window) {
        if (com.promtuz.chat.BuildConfig.CAPTURABLE) return
        val lease = leases.getOrPut(window) {
            Lease(0, window.attributes.flags and WindowManager.LayoutParams.FLAG_SECURE != 0)
        }
        lease.count++
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }

    fun holds(window: Window) = leases.containsKey(window)

    fun release(window: Window) {
        val lease = leases[window] ?: return
        if (--lease.count == 0) {
            leases.remove(window)
            if (!lease.previouslySecure && !ScreenSecurity.blocked) window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
        }
    }
}

/** "Block screenshots": every Promtuz window is secure, including in the recents preview. */
object ScreenSecurity {
    @Volatile var blocked = false
        private set

    fun apply(window: Window) {
        blocked = com.promtuz.chat.data.ChatPrefs.blockScreenshots
        if (blocked) window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        else if (!SecureRecoveryWindow.holds(window)) window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }
}
