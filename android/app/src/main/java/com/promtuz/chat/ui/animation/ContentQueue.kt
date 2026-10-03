package com.promtuz.chat.ui.animation

import kotlin.math.abs

/** How to reach a requested value. Omit this to coalesce labels to the latest value. */
interface ContentProgression<T> {
    /** Must make progress toward [target], returning [target] on the last step. */
    fun next(from: T, target: T): T
    fun remainingSteps(from: T, target: T): Long

    object Integers : ContentProgression<Int> {
        override fun next(from: Int, target: Int) = when {
            from < target -> from + 1
            from > target -> from - 1
            else -> target
        }

        override fun remainingSteps(from: Int, target: Int) = abs(target.toLong() - from.toLong())
    }
}

internal class ContentQueue<T>(initial: T) {
    internal data class Hop<T>(val target: T, val durationMillis: Int)

    private var settled = initial
    private var destination = initial
    private var active: Hop<T>? = null
    private var updatedWhileRunning = false

    fun offer(value: T) {
        if (value == destination) return
        destination = value
        if (active != null) updatedWhileRunning = true
    }

    /** Called only when Compose has finished all animations belonging to the hop. */
    fun complete() {
        active?.let { settled = it.target }
        active = null
    }

    fun next(durationMillis: Int, minDurationMillis: Int, progression: ContentProgression<T>?): Hop<T>? {
        require(durationMillis > 0 && minDurationMillis in 1..durationMillis)
        if (active != null) return null
        if (settled == destination) {
            updatedWhileRunning = false
            return null
        }
        val target = if (progression != null) progression.next(settled, destination) else destination
        require(target != settled) { "ContentProgression.next must advance toward the target" }
        val duration = if (progression != null) {
            // A larger backlog runs faster. Long arithmetic covers Int extremes.
            (durationMillis.toLong() / progression.remainingSteps(settled, destination).coerceAtLeast(1))
                .coerceIn(minDurationMillis.toLong(), durationMillis.toLong()).toInt()
        } else if (updatedWhileRunning) minDurationMillis else durationMillis
        updatedWhileRunning = false
        return Hop(target, duration).also { active = it }
    }
}
