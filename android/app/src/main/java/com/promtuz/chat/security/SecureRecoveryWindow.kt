package com.promtuz.chat.security

import com.promtuz.chat.data.ChatPrefs
import android.view.Window
import android.view.WindowManager
import java.util.WeakHashMap

/** Main-thread leases also cover overlapping screens during navigation animations. */
internal object SecureRecoveryWindow {
    private val leases = WeakHashMap<Window, Int>()

    fun acquire(window: Window) {
        if (com.promtuz.chat.BuildConfig.CAPTURABLE) return
        leases[window] = (leases[window] ?: 0) + 1
        window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }

    fun holds(window: Window) = leases.containsKey(window)

    fun release(window: Window) {
        val count = leases[window] ?: return
        if (count > 1) { leases[window] = count - 1; return }
        leases.remove(window)
        // The setting as it is now, not as it was at acquire: it may have been turned off meanwhile.
        if (!ScreenSecurity.blocked) window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }
}

/** "Block screenshots": every Promtuz window is secure, including in the recents preview. */
object ScreenSecurity {
    @Volatile var blocked = false
        private set

    fun apply(window: Window) {
        blocked = ChatPrefs.blockScreenshots
        if (blocked) window.addFlags(WindowManager.LayoutParams.FLAG_SECURE)
        else if (!SecureRecoveryWindow.holds(window)) window.clearFlags(WindowManager.LayoutParams.FLAG_SECURE)
    }
}
