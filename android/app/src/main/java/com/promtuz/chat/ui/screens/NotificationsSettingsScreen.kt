package com.promtuz.chat.ui.screens

import android.content.Intent
import android.provider.Settings
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.selection.selectable
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.data.NotifBuzz
import com.promtuz.chat.ui.components.FlexibleScreen

@Composable
fun NotificationsSettingsScreen() {
    val context = LocalContext.current
    var enabled by remember { mutableStateOf(ChatPrefs.notifEnabled) }
    var preview by remember { mutableStateOf(ChatPrefs.notifPreview) }
    var buzz by remember { mutableStateOf(ChatPrefs.notifBuzz) }
    val options = listOf(
        NotifBuzz.EveryMessage to "Every message",
        NotifBuzz.Throttled to "Every message (throttled)",
        NotifBuzz.FirstOnly to "Only first per chat",
    )

    FlexibleScreen({ Text("Notifications") }) { padding, _ ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding)
                .padding(horizontal = 20.dp),
        ) {
            SectionLabel("Alerts")
            SwitchRow("Message notifications", enabled) {
                enabled = it
                ChatPrefs.notifEnabled = it
            }
            if (enabled) {
                Text(
                    "Buzz",
                    Modifier.padding(top = 8.dp, bottom = 2.dp),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    style = MaterialTheme.typography.bodyMedium,
                )
                options.forEach { (mode, label) ->
                    Row(
                        Modifier
                            .fillMaxWidth()
                            .selectable(selected = mode == buzz) {
                                buzz = mode
                                ChatPrefs.notifBuzz = mode
                            }
                            .padding(vertical = 6.dp),
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        RadioButton(selected = mode == buzz, onClick = null)
                        Text(label)
                    }
                }
            }

            SectionLabel("Privacy")
            SwitchRow("Message preview", preview) {
                preview = it
                ChatPrefs.notifPreview = it
            }
            Text(
                "Off shows only \"New message\" on the lock screen — no sender or text.",
                Modifier.padding(top = 2.dp, bottom = 8.dp),
                color = MaterialTheme.colorScheme.onSurfaceVariant,
                style = MaterialTheme.typography.bodySmall,
            )

            // OS owns the master toggle, per-channel controls, and the re-enable path when the in-chat
            // primer was denied (the runtime prompt can't be re-summoned) — deep-link out, don't reimplement.
            OutlinedButton(
                onClick = {
                    context.startActivity(
                        Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                            .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName)
                    )
                },
                modifier = Modifier.padding(top = 20.dp),
            ) { Text("Open system notification settings") }
        }
    }
}

// Copied from ChatAppearanceScreen (both private there; concurrent session owns that file).
@Composable
private fun SectionLabel(text: String) {
    Text(
        text.uppercase(),
        Modifier.padding(top = 22.dp, bottom = 8.dp, start = 2.dp),
        MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.labelMedium,
    )
}

@Composable
private fun SwitchRow(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
    Row(
        Modifier.fillMaxWidth().padding(vertical = 6.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(label, style = MaterialTheme.typography.bodyMedium)
        Switch(checked, onChange)
    }
}
