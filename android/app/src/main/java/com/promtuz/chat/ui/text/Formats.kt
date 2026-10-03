package com.promtuz.chat.ui.text

import android.content.Context
import android.text.format.DateUtils
import com.promtuz.core.CoreBridge
import java.text.DateFormat
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.TimeZone
import uniffi.core.TimeBucket

/** m:ss, or h:mm:ss from an hour. */
fun clock(ms: Long): String {
    val s = ms / 1000
    val h = s / 3600
    return if (h > 0) "%d:%02d:%02d".format(h, s / 60 % 60, s % 60) else "%d:%02d".format(s / 60, s % 60)
}

/** Follows the device's 12/24-hour setting. */
fun timeOfDay(context: Context, ms: Long): String =
    DateUtils.formatDateTime(context, ms, DateUtils.FORMAT_SHOW_TIME)

/** The year shows only when it isn't this year. */
fun dateAndTime(context: Context, ms: Long): String = DateUtils.formatDateTime(
    context, ms, DateUtils.FORMAT_SHOW_DATE or DateUtils.FORMAT_SHOW_TIME or DateUtils.FORMAT_ABBREV_MONTH,
)

/** Core picks the bucket; the platform formatters follow the reader's locale and 12/24-hour setting. */
fun parseMessageDate(context: Context, timestamp: Long): String {
    val date = Date(timestamp)
    val offset = TimeZone.getDefault().getOffset(timestamp) / 1000
    return when (CoreBridge.timeBucket(timestamp, System.currentTimeMillis(), offset)) {
        TimeBucket.TODAY -> timeOfDay(context, timestamp)
        TimeBucket.YESTERDAY -> "Yesterday"
        TimeBucket.THIS_WEEK -> SimpleDateFormat("EEE", Locale.getDefault()).format(date)
        TimeBucket.THIS_YEAR -> SimpleDateFormat("MMM d", Locale.getDefault()).format(date)
        TimeBucket.OLDER -> DateFormat.getDateInstance(DateFormat.MEDIUM).format(date)
    }
}
