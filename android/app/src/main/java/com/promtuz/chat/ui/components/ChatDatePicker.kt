package com.promtuz.chat.ui.components

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.selected
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz
import java.time.LocalDate
import java.time.YearMonth
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle
import java.time.format.TextStyle as DateTextStyle
import java.time.temporal.WeekFields
import java.util.Locale
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

private val MonthLabel = TextStyle(fontSize = 16.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium)
private val DayLabel = TextStyle(fontSize = 15.sp, lineHeight = 20.sp)
private val WeekdayLabel = TextStyle(fontSize = 12.sp, lineHeight = 16.sp, fontWeight = FontWeight.Medium)
private val Earliest = YearMonth.of(1970, 1)

/** A month calendar on the app's glass; days after [today] can't be picked. */
@Composable
internal fun ChatDatePicker(
    initialDate: LocalDate,
    today: LocalDate,
    onDismiss: () -> Unit,
    onJump: suspend (LocalDate) -> Boolean,
) {
    val c = Promtuz.colors
    val locale = Locale.getDefault()
    val firstDay = remember(locale) { WeekFields.of(locale).firstDayOfWeek }
    var selected by remember { mutableStateOf(minOf(initialDate, today)) }
    var month by remember { mutableStateOf(YearMonth.from(selected)) }
    val latest = YearMonth.from(today)
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    val jump: () -> Unit = {
        busy = true
        error = null
        scope.launch {
            try {
                if (onJump(selected)) onDismiss() else error = "No messages in this chat."
            } catch (e: CancellationException) { throw e }
            catch (_: Exception) { error = "Couldn't open that date. Try again." }
            finally { busy = false }
        }
    }
    val monthFormat = remember(locale) { DateTimeFormatter.ofPattern("LLLL yyyy", locale) }
    val dayFormat = remember(locale) { DateTimeFormatter.ofLocalizedDate(FormatStyle.LONG).withLocale(locale) }

    AppModal(onDismiss, maxWidth = 360.dp) {
        Column(Modifier.floatingSurface(ModalShape).padding(top = 24.dp)) {
            Text("Jump to date", Modifier.padding(horizontal = 24.dp),
                style = MaterialTheme.typography.titleLargeEmphasized.copy(fontWeight = FontWeight.Medium), color = c.text)
            Row(Modifier.fillMaxWidth().padding(start = 24.dp, end = 12.dp, top = 12.dp), verticalAlignment = Alignment.CenterVertically) {
                Text(month.format(monthFormat).replaceFirstChar { it.titlecase(locale) }, Modifier.weight(1f),
                    style = MonthLabel, color = c.text)
                IconButton(onClick = { month = month.minusMonths(1) }, enabled = month > Earliest) {
                    MorphIcon(MorphGlyph.ChevronLeft, "Previous month", Modifier.size(18.dp),
                        tint = if (month > Earliest) c.text else c.muted.copy(alpha = 0.4f))
                }
                IconButton(onClick = { month = month.plusMonths(1) }, enabled = month < latest) {
                    MorphIcon(MorphGlyph.ChevronRight, "Next month", Modifier.size(18.dp),
                        tint = if (month < latest) c.text else c.muted.copy(alpha = 0.4f))
                }
            }
            Row(Modifier.fillMaxWidth().padding(horizontal = 12.dp, vertical = 4.dp)) {
                repeat(7) { i ->
                    Text(firstDay.plus(i.toLong()).getDisplayName(DateTextStyle.NARROW_STANDALONE, locale),
                        Modifier.weight(1f), style = WeekdayLabel, color = c.muted,
                        textAlign = androidx.compose.ui.text.style.TextAlign.Center)
                }
            }
            AnimatedContent(month, Modifier.padding(horizontal = 12.dp), label = "calendar month", transitionSpec = {
                val dir = if (targetState > initialState) 1 else -1
                (slideInHorizontally(tween(220)) { it / 4 * dir } + fadeIn(tween(220))) togetherWith
                    (slideOutHorizontally(tween(180)) { -it / 4 * dir } + fadeOut(tween(180)))
            }) { shown ->
                val lead = (shown.atDay(1).dayOfWeek.value - firstDay.value + 7) % 7
                Column {
                    for (week in 0 until 6) Row(Modifier.fillMaxWidth()) {
                        for (col in 0 until 7) {
                            val day = week * 7 + col - lead + 1
                            Box(Modifier.weight(1f).aspectRatio(1f), contentAlignment = Alignment.Center) {
                                if (day in 1..shown.lengthOfMonth()) {
                                    val date = shown.atDay(day)
                                    val pickable = !date.isAfter(today)
                                    val on = date == selected
                                    Box(
                                        Modifier.size(40.dp).clip(CircleShape)
                                            .background(if (on) c.accentFill else Color.Transparent)
                                            .clickable(enabled = pickable, role = Role.RadioButton) { selected = date; error = null }
                                            .semantics { this.selected = on; contentDescription = date.format(dayFormat) },
                                        contentAlignment = Alignment.Center,
                                    ) {
                                        Text("$day", style = if (date == today) DayLabel.copy(fontWeight = FontWeight.SemiBold) else DayLabel,
                                            color = when {
                                                on -> Color.White
                                                !pickable -> c.muted.copy(alpha = 0.45f)
                                                date == today -> c.accent
                                                else -> c.text
                                            })
                                    }
                                }
                            }
                        }
                    }
                }
            }
            error?.let { Text(it, Modifier.padding(horizontal = 24.dp), style = FootnoteStyle, color = c.danger) }
            ModalActions(
                confirmButton = {
                    TextButton(enabled = !busy, onClick = jump) { Text(if (busy) "Opening…" else "Jump") }
                },
                dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
            )
        }
    }
}
