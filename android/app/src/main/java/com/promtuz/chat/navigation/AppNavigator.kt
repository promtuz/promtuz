package com.promtuz.chat.navigation

import android.content.Context
import android.content.Intent
import android.os.SystemClock
import androidx.navigation3.runtime.NavKey

class AppNavigator(val backStack: MutableList<NavKey>) {
    private var lastGrowAt = 0L

    fun push(key: NavKey) {
        if (backStack.size > 1 && backStack[backStack.size - 2] == key) {
            backStack.removeLastOrNull()
        } else if (backStack.last() != key) {
            // Multi-touch / double-tap fires two forward pushes ~1 frame apart, each for a different
            // key, so both slip past the top-check and the stack grows twice. Only the first should
            // land. ponytail: 300ms accidental-tap window, not a click-throttle — a deliberate second
            // nav can't be perceived-and-tapped that fast.
            val now = SystemClock.uptimeMillis()
            if (now - lastGrowAt < 300) return
            lastGrowAt = now
            backStack.add(key)
        }
    }

    fun back(): Boolean {
        if (backStack.size > 1) {
            backStack.removeLastOrNull()
            return true
        }
        return false
    }

    /** OS intents must not be dropped by the touch debounce or duplicate an existing destination. */
    fun openExternal(key: NavKey) {
        val existing = backStack.indexOfLast { it == key }
        if (existing >= 0) {
            while (backStack.lastIndex > existing) backStack.removeLastOrNull()
        } else {
            backStack.add(key)
        }
        lastGrowAt = SystemClock.uptimeMillis()
    }

    /** Replace the whole stack with a single destination — no back path to what was there. */
    fun reset(key: NavKey) {
        backStack.clear()
        backStack.add(key)
    }
}


fun Context.goTo(clazz: Class<*>) {
    return this.startActivity(Intent(this, clazz))
}
