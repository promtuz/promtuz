package com.promtuz.chat.utils.extensions

inline fun <reified T : Any> T.structuralEquals(other: Any?): Boolean {
    if (this === other) return true
    if (other !is T) return false

    for (f in T::class.java.declaredFields) {
        f.isAccessible = true
        val a = f.get(this)
        val b = f.get(other)

        if (a is ByteArray && b is ByteArray) {
            if (!a.contentEquals(b)) return false
        } else {
            if (a != b) return false
        }
    }
    return true
}

inline fun <reified T : Any> T.structuralHash(): Int {
    var r = 1
    for (f in T::class.java.declaredFields) {
        f.isAccessible = true
        val v = f.get(this)

        r = 31 * r + when (v) {
            is ByteArray -> v.contentHashCode()
            else -> v?.hashCode() ?: 0
        }
    }
    return r
}