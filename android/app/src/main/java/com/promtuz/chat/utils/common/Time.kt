package com.promtuz.chat.utils.common

import com.promtuz.core.CoreBridge
import java.text.DateFormat
import java.text.SimpleDateFormat
import java.util.Calendar
import java.util.Date
import java.util.Locale
import java.util.TimeZone
import uniffi.core.TimeBucket

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

/**
 * How a chat list writes a timestamp.
 *
 * Which bucket it falls in is a product decision and lives in core, so iOS
 * makes the same call. Turning a bucket into text stays here: a platform date
 * formatter honours the reader's locale, calendar and 24-hour preference, none
 * of which core should try to reimplement.
 */
fun parseMessageDate(timestamp: Long, full: Boolean = true): String {
    val date = Date(timestamp)
    if (!full) return DateFormat.getTimeInstance(DateFormat.SHORT).format(date)

    val offset = TimeZone.getDefault().getOffset(timestamp) / 1000
    return when (CoreBridge.timeBucket(timestamp, System.currentTimeMillis(), offset)) {
        TimeBucket.TODAY -> DateFormat.getTimeInstance(DateFormat.SHORT).format(date)
        TimeBucket.YESTERDAY -> "Yesterday"
        TimeBucket.THIS_WEEK -> SimpleDateFormat("EEE", Locale.getDefault()).format(date)
        TimeBucket.THIS_YEAR -> SimpleDateFormat("MMM d", Locale.getDefault()).format(date)
        TimeBucket.OLDER -> DateFormat.getDateInstance(DateFormat.MEDIUM).format(date)
    }
}
