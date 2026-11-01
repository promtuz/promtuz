package com.promtuz.chat.security

/** No wall clock: deadlines survive sleep and are checked again before revealing content. */
internal class PrivateSessionPolicy {
    var armed = false
        private set
    private var deadline: Long? = null
    private var departed = false

    fun arm() { armed = true }
    fun clear() { armed = false; deadline = null; departed = false }
    fun handoff(now: Long) {
        if (armed && deadline == null) deadline = now + HANDOFF_MS
    }
    fun remaining(now: Long): Long = deadline?.let { (it - now).coerceAtLeast(0) } ?: HANDOFF_MS
    fun depart(now: Long): Boolean {
        if (!armed) return false
        departed = true
        return if (deadline?.let { now < it } == true) false else lock()
    }
    fun resume(now: Long): Boolean {
        if (expire(now)) return true
        if (departed) { deadline = null; departed = false }
        return false
    }
    fun expire(now: Long): Boolean = if (armed && deadline?.let { now >= it } == true) lock() else false
    fun screenOff(): Boolean = if (armed) lock() else false
    private fun lock(): Boolean { clear(); return true }
    companion object { const val HANDOFF_MS = 60_000L }
}
