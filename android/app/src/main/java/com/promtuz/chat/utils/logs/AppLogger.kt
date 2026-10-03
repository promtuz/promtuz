package com.promtuz.chat.utils.logs

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import timber.log.Timber
import java.util.Calendar
import java.util.concurrent.atomic.AtomicLong

data class AppLog(
    val time: Long,
    val priority: Int,
    val tag: String?,
    val message: String,
    val t: Throwable?,
) {
    // A unique LazyColumn key, since content and time can repeat; as a body val it stays out of equals.
    val id: Long = nextId.getAndIncrement()

    companion object {
        private val nextId = AtomicLong(0)

        fun charPriority(char: Char) = when (char) {
            'V' -> 2
            'D' -> 3
            'I' -> 4
            'W' -> 5
            'E' -> 6
            'F' -> 7
            else -> 3
        }
    }
}

object AppLogger : Timber.Tree() {
    private const val MAX_ENTRIES = 2_000
    @Volatile var minimumPriority = 4

    override fun isLoggable(tag: String?, priority: Int) = priority >= minimumPriority

    override fun log(
        priority: Int,
        tag: String?,
        message: String,
        t: Throwable?
    ) {
        val time = Calendar.getInstance().timeInMillis
        val cleanMessage = if (t != null) message.substringBefore('\n') else message

        this.push(AppLog(time, priority, tag, cleanMessage, t))
    }

    fun push(log: AppLog) {
        if (log.priority < minimumPriority) return
        _logs.update { listOf(log) + it.take(MAX_ENTRIES - 1) }
    }

    fun clear() { _logs.value = emptyList() }

    private var _logs = MutableStateFlow(emptyList<AppLog>())
    val logs = _logs.asStateFlow()
}
