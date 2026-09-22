package com.promtuz.chat.ui.components

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import com.promtuz.chat.update.ReleaseNote
import java.time.LocalDate
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle

/** "What's new": every release after the installed build up to [offered], newest first. */
fun LazyListScope.releaseNotes(notes: List<ReleaseNote>, offered: String) {
    if (notes.isEmpty()) return
    item("notes") {
        Column(Modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(24.dp)) {
            Text("What's new", style = MaterialTheme.typography.titleLarge)
            notes.forEach { ReleaseNoteCard(it, current = it.version == offered) }
        }
    }
}

@Composable
private fun ReleaseNoteCard(note: ReleaseNote, current: Boolean) {
    val colors = MaterialTheme.colorScheme
    Column(Modifier.fillMaxWidth()) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(
                note.version,
                Modifier
                    .background(if (current) colors.primaryContainer else colors.surfaceContainerHigh, RoundedCornerShape(8.dp))
                    .padding(10.dp, 4.dp),
                style = MaterialTheme.typography.labelLarge,
                color = if (current) colors.onPrimaryContainer else colors.onSurface,
            )
            Text(niceDate(note.date), style = MaterialTheme.typography.bodySmall, color = colors.onSurfaceVariant)
        }
        Spacer(Modifier.height(8.dp))
        NotesBody(note.body)
    }
}

/** Headings, bullets, paragraphs and `**bold**`; enough for release notes. */
@Composable
private fun NotesBody(markdown: String) {
    val body = MaterialTheme.typography.bodyMedium
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        for (line in markdown.lines().map { it.trim() }.filter { it.isNotEmpty() }) {
            when {
                line.startsWith("#") -> Text(
                    line.trimStart('#').trim(),
                    Modifier.padding(top = 6.dp),
                    style = MaterialTheme.typography.titleMedium,
                )
                line.startsWith("- ") || line.startsWith("* ") -> Row {
                    Text("•", Modifier.padding(end = 8.dp), style = body)
                    Text(inline(line.drop(2)), style = body)
                }
                else -> Text(inline(line), style = body)
            }
        }
    }
}

private fun inline(text: String): AnnotatedString = buildAnnotatedString {
    var rest = text
    while (true) {
        val open = rest.indexOf("**")
        val close = if (open >= 0) rest.indexOf("**", open + 2) else -1
        if (close < 0) { append(rest); break }
        append(rest.substring(0, open))
        withStyle(SpanStyle(fontWeight = FontWeight.SemiBold)) { append(rest.substring(open + 2, close)) }
        rest = rest.substring(close + 2)
    }
}

private fun niceDate(date: String): String = runCatching {
    LocalDate.parse(date).format(DateTimeFormatter.ofLocalizedDate(FormatStyle.MEDIUM))
}.getOrDefault(date)
