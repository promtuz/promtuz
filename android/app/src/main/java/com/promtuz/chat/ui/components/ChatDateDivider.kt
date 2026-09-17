package com.promtuz.chat.ui.components

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.icu.text.DateFormat
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.ui.appearance.LocalChatColors
import java.time.LocalDate
import java.time.ZoneId
import java.util.Date

internal data class ChatCalendar(val today: LocalDate, val zone: ZoneId)

@Composable
internal fun rememberChatCalendar(): ChatCalendar {
    val context = LocalContext.current
    fun current(): ChatCalendar {
        val zone = ZoneId.systemDefault()
        return ChatCalendar(LocalDate.now(zone), zone)
    }
    var calendar by remember { mutableStateOf(current()) }
    LifecycleResumeEffect(context) {
        calendar = current()
        onPauseOrDispose { }
    }
    DisposableEffect(context) {
        val receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context?, intent: Intent?) {
                calendar = current()
            }
        }
        val filter = IntentFilter().apply {
            addAction(Intent.ACTION_DATE_CHANGED)
            addAction(Intent.ACTION_TIME_CHANGED)
            addAction(Intent.ACTION_TIMEZONE_CHANGED)
        }
        ContextCompat.registerReceiver(context, receiver, filter, ContextCompat.RECEIVER_NOT_EXPORTED)
        onDispose { context.unregisterReceiver(receiver) }
    }
    return calendar
}

@Composable
internal fun ChatDateDivider(date: LocalDate, today: LocalDate) {
    val colors = LocalChatColors.current
    val locale = LocalConfiguration.current.locales[0]
    val label = remember(date, today, locale) {
        when (date) {
            today -> "Today"
            today.minusDays(1) -> "Yesterday"
            else -> DateFormat.getInstanceForSkeleton(
                if (date.year == today.year) "MMMMd" else "yMMMMd", locale,
            ).apply { timeZone = android.icu.util.TimeZone.getTimeZone("UTC") }
                .format(Date(date.toEpochDay() * 86_400_000L))
        }
    }
    Box(
        Modifier.fillMaxWidth().padding(horizontal = 16.dp, vertical = 10.dp),
        contentAlignment = Alignment.Center,
    ) {
        Text(
            label,
            modifier = Modifier
                .background(colors.bar.copy(alpha = 0.9f), CircleShape)
                .padding(horizontal = 12.dp, vertical = 5.dp)
                .semantics { heading() },
            style = MaterialTheme.typography.labelMedium,
            color = colors.marker,
        )
    }
}
