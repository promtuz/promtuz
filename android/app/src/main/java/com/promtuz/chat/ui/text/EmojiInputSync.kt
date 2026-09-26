package com.promtuz.chat.ui.text

/** Acknowledgements of earlier IME edits must not overwrite more recent keystrokes. */
internal class EmojiInputSync(initialValue: String) {
    private var external = initialValue
    private var initial = true
    private val pending = ArrayList<String>()

    fun publish(text: String) {
        if (text != (pending.lastOrNull() ?: external)) pending += text
    }

    /** True only for a new programmatic draft, rather than an echo of an IME edit. */
    fun acceptExternal(text: String): Boolean {
        val first = initial
        initial = false
        // Initialize the native editor once, unless typing has already started.
        if (text == external) return first && pending.isEmpty()
        external = text
        val acknowledged = pending.indexOfLast { it == text }
        if (acknowledged >= 0) {
            pending.subList(0, acknowledged + 1).clear()
            return false
        }
        pending.clear()
        return true
    }
}
