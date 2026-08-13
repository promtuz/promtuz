package com.promtuz.chat.utils.extensions

import uniffi.core.CoreException

/**
 * The part of a core failure worth showing someone.
 *
 * uniffi builds `message` out of the variant's field names, so a
 * `CoreError::Internal { msg }` reaches Kotlin reading `msg=…` — the field
 * label leaks into the UI. Read the field itself instead, and fall back to
 * [fallback] for anything that isn't a core error or carries no text at all.
 */
fun Throwable.reason(fallback: String): String {
    val text = when (this) {
        is CoreException.Internal -> msg
        else -> message
    }
    return text?.trim()?.takeIf { it.isNotEmpty() } ?: fallback
}
