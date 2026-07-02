package com.promtuz.chat.navigation

import android.content.Context
import android.content.Intent
import androidx.navigation3.runtime.NavKey

class AppNavigator(val backStack: MutableList<NavKey>) {
    fun push(key: NavKey) {
        if (backStack.size > 1 && backStack[backStack.size - 2] == key) {
            backStack.removeLastOrNull()
        } else if (backStack.last() != key) backStack.add(key)
    }

    fun back(): Boolean {
        if (backStack.size > 1) {
            backStack.removeLastOrNull()
            return true
        }
        return false
    }
}


fun Context.goTo(clazz: Class<*>) {
    return this.startActivity(Intent(this, clazz))
}