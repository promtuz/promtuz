package com.promtuz.chat.navigation

import android.os.SystemClock
import androidx.navigation3.runtime.NavKey

class AppNavigator(val backStack: MutableList<NavKey>) {
    private var lastGrowAt = 0L

    fun push(key: NavKey) {
        if (backStack.size > 1 && backStack[backStack.size - 2] == key) {
            backStack.removeLastOrNull()
        } else if (backStack.last() != key) {
            // A double tap can push two different keys a frame apart; only the first should land.
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

    fun reset(key: NavKey) {
        backStack.clear()
        backStack.add(key)
    }
}
