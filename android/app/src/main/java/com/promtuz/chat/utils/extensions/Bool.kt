package com.promtuz.chat.utils.extensions

inline fun Boolean.then(block: () -> Unit): Boolean {
    if (this) block()
    return this
}