package com.promtuz.chat.utils.logs

import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import timber.log.Timber
import java.util.Calendar

data class AppLog(
    val time: Long,
    val priority: Int,
    val tag: String?,
    val message: String,
    val t: Throwable?,
) {
    companion object {
        /**
         * Returns logcat priority based on given character
         */
        fun charPriority(char: Char) = when (char) {
            'V' -> 2
            'D' -> 3
            'I' -> 4
            'W' -> 5
            'E' -> 6
            else -> 3
        }
    }
}

object AppLogger : Timber.Tree() {
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
        _logs.update { listOf(log) + it }
    }

    private var _logs = MutableStateFlow(emptyList<AppLog>())
    val logs = _logs.asStateFlow()
}