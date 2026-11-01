package com.promtuz.chat.ui.animation

import kotlin.math.abs
import kotlin.math.floor

/** How values are ordered and sampled in a scrolling strip. Labels can omit this. */
interface ContentProgression<T> {
    fun next(from: T, target: T): T
    fun remainingSteps(from: T, target: T): Long

    /** Positive values scroll upward; negative values scroll downward. */
    fun direction(from: T, target: T): Int = 1

    /** Excludes [from], includes [target], and returns at most [maxItems] values. */
    fun sample(from: T, target: T, maxItems: Int): List<T> {
        require(maxItems > 0)
        if (from == target) return emptyList()
        val result = ArrayList<T>()
        var current = from
        repeat(maxItems - 1) {
            val step = next(current, target)
            require(step != current) { "ContentProgression.next must advance toward the target" }
            result += step
            current = step
            if (current == target) return result
        }
        result += target
        return result
    }

    object Integers : ContentProgression<Int> {
        override fun next(from: Int, target: Int) = when {
            from < target -> from + 1
            from > target -> from - 1
            else -> target
        }
        override fun remainingSteps(from: Int, target: Int) = abs(target.toLong() - from.toLong())
        override fun direction(from: Int, target: Int) = target.compareTo(from)

        override fun sample(from: Int, target: Int, maxItems: Int): List<Int> {
            require(maxItems > 0)
            val distance = target.toLong() - from.toLong()
            val count = minOf(abs(distance), maxItems.toLong()).toInt()
            if (count == 0) return emptyList()
            // Long arithmetic covers the full Int range; sample evenly without walking it.
            return List(count) { index -> (from.toLong() + distance * (index + 1) / count).toInt() }
        }
    }
}

/** Bounded strip planner. Only the two currently visible entries survive a retarget. */
internal class ContentQueue<T>(initial: T) {
    data class Entry<T>(val id: Long, val value: T)
    data class Sweep<T>(val entries: List<Entry<T>>, val start: Float, val target: Float)

    private var nextId = 0L
    var entries = listOf(entry(initial))
        private set

    private fun entry(value: T) = Entry(nextId++, value)

    fun retarget(position: Float, value: T, progression: ContentProgression<T>?, maxItems: Int): Sweep<T> {
        require(maxItems > 0)
        val offset = position.coerceIn(0f, entries.lastIndex.toFloat())
        val first = floor(offset).toInt()
        val fraction = offset - first
        val visible = entries.subList(first, minOf(first + 2, entries.size)).toList()
        val existing = visible.indexOfFirst { it.value == value }
        if (existing >= 0) {
            entries = visible
            return Sweep(entries, fraction, existing.toFloat())
        }
        val prepend = progression != null && progression.direction(visible.first().value, value) < 0
        val from = if (prepend) visible.first().value else visible.last().value
        val samples = progression?.sample(from, value, maxItems) ?: listOf(value)
        require(samples.isNotEmpty() && samples.last() == value && samples.size <= maxItems) {
            "ContentProgression.sample must end at the target within its item budget"
        }
        val added = samples.map(::entry)
        entries = if (prepend) added.asReversed() + visible else visible + added
        return Sweep(entries,
            start = fraction + if (prepend) added.size else 0,
            target = if (prepend) 0f else entries.lastIndex.toFloat())
    }

    fun settle(value: T): List<Entry<T>> {
        entries = listOf(entries.firstOrNull { it.value == value } ?: entry(value))
        return entries
    }
}
