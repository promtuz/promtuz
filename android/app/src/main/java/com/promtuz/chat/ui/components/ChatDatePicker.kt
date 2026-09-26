package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import java.time.Instant
import java.time.LocalDate
import java.time.ZoneOffset
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch

@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun ChatDatePicker(
    initialDate: LocalDate,
    today: LocalDate,
    onDismiss: () -> Unit,
    onJump: suspend (LocalDate) -> Boolean,
) {
    // Material's date values are UTC calendar dates, not local instants.
    val state = rememberDatePickerState(
        initialSelectedDateMillis = initialDate.toEpochDay() * 86_400_000L,
        yearRange = 1970..maxOf(today.year, initialDate.year),
        selectableDates = remember(today) { object : SelectableDates {
            override fun isSelectableDate(utcTimeMillis: Long) = utcTimeMillis <= today.toEpochDay() * 86_400_000L
            override fun isSelectableYear(year: Int) = year <= today.year
        } },
    )
    val scope = rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var error by remember { mutableStateOf<String?>(null) }
    DatePickerDialog(
        colors = DatePickerDefaults.colors(containerColor = MaterialTheme.colorScheme.surfaceContainer),
        onDismissRequest = onDismiss,
        confirmButton = {
            TextButton(enabled = state.selectedDateMillis != null && !busy, onClick = {
                val date = Instant.ofEpochMilli(state.selectedDateMillis!!).atZone(ZoneOffset.UTC).toLocalDate()
                busy = true
                error = null
                scope.launch {
                    try {
                        if (onJump(date)) onDismiss() else error = "No messages in this chat."
                    } catch (e: CancellationException) { throw e }
                    catch (_: Exception) { error = "Couldn't open that date. Try again." }
                    finally { busy = false }
                }
            }) { Text(if (busy) "Opening…" else "Jump") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("Cancel") } },
    ) {
        ModalWindowMotion()
        Column {
            DatePicker(state, colors = DatePickerDefaults.colors(containerColor = MaterialTheme.colorScheme.surfaceContainer), title = { Text("Jump to date", Modifier.padding(start = 24.dp, top = 16.dp)) })
            error?.let { Text(it, Modifier.padding(24.dp), color = MaterialTheme.colorScheme.error) }
        }
    }
}
