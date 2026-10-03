package com.promtuz.chat.utils.extensions

import uniffi.core.CoreException

/** UniFFI's `message` embeds the field label (`msg=...`), so this reads the field itself. */
fun Throwable.reason(fallback: String): String {
    val text = when (this) {
        is CoreException.Internal -> msg
        is CoreException.Refused -> msg
        else -> message
    }
    return text?.trim()?.takeIf { it.isNotEmpty() } ?: fallback
}
