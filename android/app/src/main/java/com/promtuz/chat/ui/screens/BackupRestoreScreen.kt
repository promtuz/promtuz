package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.ui.components.TextButton
import com.promtuz.chat.ui.components.GroupedScreen
import android.text.format.DateUtils
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import android.text.format.Formatter
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.ui.Alignment
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Promtuz
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.BackupOutcome
import com.promtuz.chat.presentation.viewmodel.BackupRestoreVM
import com.promtuz.chat.ui.components.*
import org.koin.androidx.compose.koinViewModel

/** Restore is additive: it only adds rows it doesn't already have. */
@Composable
fun BackupRestoreScreen(viewModel: BackupRestoreVM = koinViewModel()) {
    val c = Promtuz.colors
    val context = LocalContext.current
    val lastBackup by viewModel.lastBackup.collectAsState()
    val bytes by viewModel.bytes.collectAsState()
    val busy by viewModel.busy.collectAsState()
    val outcome by viewModel.outcome.collectAsState()

    val saveCopy = rememberLauncherForActivityResult(
        ActivityResultContracts.CreateDocument("application/octet-stream")
    ) { uri -> uri?.let(viewModel::saveCopyTo) }

    val pickBackup = rememberLauncherForActivityResult(
        ActivityResultContracts.OpenDocument()
    ) { uri -> uri?.let(viewModel::restoreFrom) }

    GroupedScreen({ Text("Backup") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            Row(
                Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.card).padding(16.dp),
                horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically,
            ) {
                val tint = if (lastBackup != null) c.success else c.muted
                Box(
                    Modifier.size(48.dp).clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.badge)).background(tint.copy(alpha = 0.16f)),
                    contentAlignment = Alignment.Center,
                ) { DrawableIcon(R.drawable.i_chat_backup, size = 24.dp, tint = tint) }
                Column(verticalArrangement = Arrangement.spacedBy(2.dp)) {
                    Text(lastBackup?.let { "Backed up ${relative(it)}" } ?: "No backup yet",
                        style = TextStyle(fontSize = 17.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium), color = c.text)
                    Text(
                        if (lastBackup != null) "${Formatter.formatShortFileSize(context, bytes)} · updates when you leave the app"
                        else "Promtuz backs up when you leave the app",
                        style = RowSubtitleStyle, color = c.muted,
                    )
                }
            }
            CardGap()
            AppButton("Back up now", viewModel::snapshot, Modifier.fillMaxWidth(), busy = busy)
            CardGap()
            SettingsCard(
                header = "Move to another phone",
                footer = "Backups are encrypted with your recovery phrase. Without it, nobody can open them, including you.",
            ) {
                SettingsRow(
                    "Save a copy", subtitle = "Export an encrypted file", icon = R.drawable.oi_export,
                    trailing = Trailing.Chevron, enabled = !busy && lastBackup != null,
                    onClick = { saveCopy.launch(viewModel.suggestedFileName()) },
                )
                SettingsRow(
                    "Restore from a file", subtitle = "Adds chats and messages missing here", icon = R.drawable.i_download,
                    trailing = Trailing.Chevron, enabled = !busy,
                    onClick = { pickBackup.launch(arrayOf("*/*")) },
                )
            }
        }
    }

    outcome?.let { result ->
        val (title, text) = when (result) {
            is BackupOutcome.Failed -> result.title to result.text
            is BackupOutcome.Restored -> restoredCopy(result)
        }
        AppAlertDialog(
            onDismissRequest = viewModel::dismissOutcome,
            title = { Text(title) },
            text = { Text(text) },
            confirmButton = { TextButton(viewModel::dismissOutcome) { Text("OK") } },
        )
    }
}

private fun relative(time: Long): String {
    val now = System.currentTimeMillis()
    return if (now - time < DateUtils.MINUTE_IN_MILLIS) "just now"
    else DateUtils.getRelativeTimeSpanString(time, now, DateUtils.MINUTE_IN_MILLIS).toString()
}

private fun restoredCopy(r: BackupOutcome.Restored): Pair<String, String> {
    fun count(n: Int, one: String) = if (n == 1) "1 $one" else "$n ${one}s"
    val parts = listOfNotNull(
        count(r.chats, "chat").takeIf { r.chats > 0 },
        count(r.contacts, "contact").takeIf { r.contacts > 0 },
        count(r.messages, "message").takeIf { r.messages > 0 },
    )
    val old = if (r.unreachable) " This backup is from an older version, so its messages couldn't be restored." else ""
    if (parts.isEmpty()) return "Nothing new to restore" to "Everything in this backup is already on this device.$old"
    val list = if (parts.size == 1) parts[0] else parts.dropLast(1).joinToString(", ") + " and " + parts.last()
    return "Backup restored" to "Added $list.$old"
}
