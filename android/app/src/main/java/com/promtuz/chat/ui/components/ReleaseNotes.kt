package com.promtuz.chat.ui.components

import com.promtuz.chat.ui.theme.Radii
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.shape.RoundedCornerShape
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
import androidx.compose.ui.unit.sp
import androidx.compose.ui.draw.clip
import com.promtuz.chat.update.ReleaseNote
import java.time.LocalDate
import java.time.format.DateTimeFormatter
import java.time.format.FormatStyle

/** Preserve authored copy, fold shared headings, and remove exact repeats across releases. */
private fun combineNotes(notes: List<ReleaseNote>): String {
    val sections = linkedMapOf<String, LinkedHashSet<String>>()
    notes.sortedByDescending { it.code }.forEach { note ->
        var heading = ""
        note.body.lineSequence().map(String::trim).filter(String::isNotEmpty).forEach { line ->
            if (line.startsWith("#")) heading = line.trimStart('#').trim()
            else sections.getOrPut(heading) { linkedSetOf() }.add(line)
        }
    }
    return sections.entries.joinToString("\n\n") { (heading, lines) ->
        (if (heading.isBlank()) "" else "### $heading\n") + lines.joinToString("\n")
    }
}

fun LazyListScope.releaseHistory(notes: List<ReleaseNote>, currentCode: Long) {
    notes.forEach { note -> item("release-${note.code}") {
        ReleaseNoteCard(note, note.code == currentCode, showBuild = true)
    } }
}

@Composable
private fun ReleaseNoteCard(note: ReleaseNote, current: Boolean, showBuild: Boolean = false) {
    val c = com.promtuz.chat.ui.theme.Promtuz.colors
    Column(Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.card).padding(16.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(
                note.version,
                Modifier
                    .background(if (current) c.accent.copy(alpha = 0.16f) else c.cardHigh, RoundedCornerShape(8.dp))
                    .padding(10.dp, 4.dp),
                style = androidx.compose.ui.text.TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = FontWeight.SemiBold),
                color = if (current) c.accent else c.text,
            )
            val detail = listOfNotNull(niceDate(note.date).takeIf { it.isNotBlank() },
                "Build ${note.code}".takeIf { showBuild }, "Installed".takeIf { current }).joinToString(" · ")
            Text(detail, style = androidx.compose.ui.text.TextStyle(fontSize = 13.sp, lineHeight = 18.sp), color = c.muted)
        }
        Spacer(Modifier.height(12.dp))
        NotesBody(note.body)
    }
}

/** An offered update's notes, folded into one list. */
@Composable
fun UpdateNotes(notes: List<ReleaseNote>) = NotesBody(combineNotes(notes))

/** Headings, bullets, paragraphs and `**bold**`; enough for release notes. */
@Composable
private fun NotesBody(markdown: String) {
    val c = com.promtuz.chat.ui.theme.Promtuz.colors
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        for (line in markdown.lines().map { it.trim() }.filter { it.isNotEmpty() }) {
            when {
                line.startsWith("#") -> Text(
                    line.trimStart('#').trim(),
                    Modifier.padding(top = 6.dp),
                    style = RowTitleStyle.copy(fontWeight = FontWeight.Medium), color = c.text,
                )
                line.startsWith("- ") || line.startsWith("* ") -> Row {
                    Box(Modifier.padding(top = 8.dp, end = 12.dp).size(5.dp).background(c.muted, androidx.compose.foundation.shape.CircleShape))
                    Text(inline(line.drop(2)), style = RowValueStyle, color = c.text)
                }
                else -> Text(inline(line), style = RowValueStyle, color = c.text)
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
    val day = LocalDate.parse(date)
    if (day.year == LocalDate.now().year) day.format(DateTimeFormatter.ofPattern("MMM d"))
    else day.format(DateTimeFormatter.ofLocalizedDate(FormatStyle.MEDIUM))
}.getOrDefault(date)
