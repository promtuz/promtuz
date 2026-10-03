package com.promtuz.chat.ui.screens

import android.app.NotificationManager
import android.content.Intent
import android.provider.Settings
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.data.NotifBuzz
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.ui.components.SettingsSection
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.core.push.Notifications

@Composable
fun NotificationsSettingsScreen() {
    val context = LocalContext.current
    val manager = remember(context) { context.getSystemService(NotificationManager::class.java) }
    var enabled by remember { mutableStateOf(ChatPrefs.notifEnabled) }
    var preview by remember { mutableStateOf(ChatPrefs.notifPreview) }
    var buzz by remember { mutableStateOf(ChatPrefs.notifBuzz) }
    var systemEnabled by remember { mutableStateOf(manager.areNotificationsEnabled()) }
    var messagesEnabled by remember {
        mutableStateOf(manager.getNotificationChannel(Notifications.MESSAGES_CHANNEL)?.importance != NotificationManager.IMPORTANCE_NONE)
    }

    // Android owns permissions and channels. Refresh after returning from its settings, including
    // when this screen stayed on the navigation stack while the activity was paused.
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) {
        systemEnabled = manager.areNotificationsEnabled()
        messagesEnabled = manager.getNotificationChannel(Notifications.MESSAGES_CHANNEL)?.importance != NotificationManager.IMPORTANCE_NONE
        enabled = ChatPrefs.notifEnabled
        preview = ChatPrefs.notifPreview
        buzz = ChatPrefs.notifBuzz
    }

    SimpleScreen({ Text("Notifications") }) { padding ->
        Column(
            Modifier.fillMaxSize()
                .verticalScroll(rememberScrollState())
                .padding(padding.listPadding()),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            GroupedActionRow(
                title = "Message notifications",
                index = 0,
                groupSize = 1,
                supportingText = if (enabled) "Notify me about new messages" else "Off for all chats. Calls are unaffected.",
                checked = enabled,
                onClick = {
                    enabled = !enabled
                    ChatPrefs.notifEnabled = enabled
                },
                control = { Switch(checked = enabled, onCheckedChange = null) },
            )

            AnimatedVisibility(visible = enabled) {
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    SettingsSection("Alert frequency")
                    Column(
                        Modifier.selectableGroup(),
                        verticalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        NotifBuzz.entries.forEachIndexed { index, mode ->
                            GroupedActionRow(
                                title = when (mode) {
                                    NotifBuzz.EveryMessage -> "Every message"
                                    NotifBuzz.Throttled -> "Fewer alerts"
                                    NotifBuzz.FirstOnly -> "First message only"
                                },
                                supportingText = when (mode) {
                                    NotifBuzz.EveryMessage -> "Alert for each new message"
                                    NotifBuzz.Throttled -> "At most one alert per chat every 2 seconds"
                                    NotifBuzz.FirstOnly -> "Alert once until that chat’s notification is cleared"
                                },
                                index = index,
                                groupSize = NotifBuzz.entries.size,
                                selected = mode == buzz,
                                onClick = {
                                    buzz = mode
                                    ChatPrefs.notifBuzz = mode
                                },
                                control = { RadioButton(selected = mode == buzz, onClick = null) },
                            )
                        }
                    }
                    SettingsSection("Privacy")
                    GroupedActionRow(
                        title = "Message previews",
                        index = 0,
                        groupSize = 1,
                        supportingText = if (preview) "Show sender names and message text" else "Show only “New message”, without names or text",
                        checked = preview,
                        onClick = {
                            preview = !preview
                            ChatPrefs.notifPreview = preview
                        },
                        control = { Switch(checked = preview, onCheckedChange = null) },
                    )
                }
            }

            SettingsSection("Android settings")
            GroupedActionRow(
                title = "Message sound & vibration",
                supportingText = when {
                    !systemEnabled -> "Blocked by Android. Allow notifications below."
                    !messagesEnabled -> "Message notifications are blocked. Tap to enable."
                    else -> "Sound, vibration and lock screen visibility"
                },
                index = 0,
                groupSize = 2,
                onClick = {
                    context.startActivity(Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName)
                        .putExtra(Settings.EXTRA_CHANNEL_ID, Notifications.MESSAGES_CHANNEL))
                },
                control = { SettingsChevron() },
            )
            GroupedActionRow(
                title = "All notification settings",
                supportingText = if (systemEnabled) "Messages, calls and app updates" else "Notifications are blocked for Promtuz. Tap to enable.",
                index = 1,
                groupSize = 2,
                onClick = {
                    context.startActivity(Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName))
                },
                control = { SettingsChevron() },
            )
        }
    }
}

@Composable
private fun SettingsChevron() {
    MorphIcon(MorphGlyph.ChevronRight, null, Modifier.size(20.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
}
