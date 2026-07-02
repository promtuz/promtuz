package com.promtuz.chat.utils.common

import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Date
import java.util.Locale

class Time {
    companion object {
        private val simpleDateFormat = SimpleDateFormat("dd MMMM yyyy, HH:mm:ss", Locale.ENGLISH)

        private fun tsToDate(ts: Long): Date = Date(ts)

        fun getDateString(time: Long): String = simpleDateFormat.format(tsToDate(time * 1000L))
        fun getDateString(time: ULong): String = getDateString(time.toLong())

        /**
         * Returns current system time in milliseconds
         */
        fun now(): Long = Calendar.getInstance().timeInMillis
    }
}

fun parseMessageDate(timestamp: Long, full: Boolean = true): String {
    val date = Date(timestamp)

    if (!full) {
        return SimpleDateFormat("HH:mm", Locale.US).format(date)
    }

    val now = Calendar.getInstance()
    val messageDate = Calendar.getInstance().apply { time = date }

    // 24 hour time if today
    // TODO: Add time display setting option in setting?
    //  or use system wide option
    if (isSameDay(messageDate, now)) {
        return SimpleDateFormat("HH:mm", Locale.US).format(date)
    }

    val yesterday = Calendar.getInstance().apply { add(Calendar.DATE, -1) }
    if (isSameDay(messageDate, yesterday)) {
        return "Yesterday"
    }

    val weekAgo = Calendar.getInstance().apply { add(Calendar.DATE, -7) }
    if (messageDate.after(weekAgo)) {
        // for eg. Wed
        return SimpleDateFormat("EEE", Locale.US).format(date)
    }

    if (messageDate.get(Calendar.YEAR) == now.get(Calendar.YEAR)) {
        // for eg. Nov, 23
        return SimpleDateFormat("MMM, d", Locale.US).format(date)
    }

    return SimpleDateFormat("MMMM d, yyyy", Locale.US).format(date)
}

private fun isSameDay(cal1: Calendar, cal2: Calendar): Boolean {
    return cal1.get(Calendar.YEAR) == cal2.get(Calendar.YEAR) &&
            cal1.get(Calendar.DAY_OF_YEAR) == cal2.get(Calendar.DAY_OF_YEAR)
}