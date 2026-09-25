package com.promtuz.chat.ui.screens

import android.app.NotificationManager
import android.content.Intent
import android.provider.Settings
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.spring
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.selection.toggleable
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
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.data.NotifBuzz
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.util.groupedRoundShape
import com.promtuz.core.push.Notifications

@Composable
fun NotificationsSettingsScreen() {
    val context = LocalContext.current
    val direction = LocalLayoutDirection.current
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
                .padding(
                    start = padding.calculateLeftPadding(direction) + 18.dp,
                    end = padding.calculateRightPadding(direction) + 18.dp,
                    top = padding.calculateTopPadding() + 12.dp,
                    bottom = padding.calculateBottomPadding() + 24.dp,
                ),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            NotificationSettingRow(
                title = "Message notifications",
                detail = if (enabled) "Notify me about new messages" else "Off for all chats. Calls are unaffected.",
                checked = enabled,
                onClick = {
                    enabled = !enabled
                    ChatPrefs.notifEnabled = enabled
                },
            ) { Switch(checked = enabled, onCheckedChange = null) }

            AnimatedVisibility(visible = enabled) {
                Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                    NotificationSection("Alert frequency")
                    Column(
                        Modifier.selectableGroup(),
                        verticalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        NotifBuzz.entries.forEachIndexed { index, mode ->
                            NotificationSettingRow(
                                title = when (mode) {
                                    NotifBuzz.EveryMessage -> "Every message"
                                    NotifBuzz.Throttled -> "Fewer alerts"
                                    NotifBuzz.FirstOnly -> "First message only"
                                },
                                detail = when (mode) {
                                    NotifBuzz.EveryMessage -> "Alert for each new message"
                                    NotifBuzz.Throttled -> "At most one alert per chat every 2 seconds"
                                    NotifBuzz.FirstOnly -> "Alert once until that chat’s notification is cleared"
                                },
                                index = index,
                                count = NotifBuzz.entries.size,
                                selected = mode == buzz,
                                onClick = {
                                    buzz = mode
                                    ChatPrefs.notifBuzz = mode
                                },
                            ) { RadioButton(selected = mode == buzz, onClick = null) }
                        }
                    }
                    NotificationSection("Privacy")
                    NotificationSettingRow(
                        title = "Message previews",
                        detail = if (preview) "Show sender names and message text" else "Show only “New message”, without names or text",
                        checked = preview,
                        onClick = {
                            preview = !preview
                            ChatPrefs.notifPreview = preview
                        },
                    ) { Switch(checked = preview, onCheckedChange = null) }
                }
            }

            NotificationSection("Android settings")
            NotificationSettingRow(
                title = "Message sound & vibration",
                detail = when {
                    !systemEnabled -> "Blocked by Android. Allow notifications below."
                    !messagesEnabled -> "Message notifications are blocked. Tap to enable."
                    else -> "Sound, vibration and lock screen visibility"
                },
                index = 0,
                count = 2,
                onClick = {
                    context.startActivity(Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName)
                        .putExtra(Settings.EXTRA_CHANNEL_ID, Notifications.MESSAGES_CHANNEL))
                },
            ) { NotificationSettingsChevron() }
            NotificationSettingRow(
                title = "All notification settings",
                detail = if (systemEnabled) "Messages, calls and app updates" else "Notifications are blocked for Promtuz. Tap to enable.",
                index = 1,
                count = 2,
                onClick = {
                    context.startActivity(Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName))
                },
            ) { NotificationSettingsChevron() }
        }
    }
}

@Composable
private fun NotificationSection(title: String) {
    Text(
        title.uppercase(),
        Modifier.padding(top = 20.dp, bottom = 3.dp, start = 2.dp).semantics { heading() },
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = avgSizeInStyle(MaterialTheme.typography.labelLargeEmphasized, MaterialTheme.typography.labelMediumEmphasized),
    )
}

/** One semantic control per row, with the same press feedback on the label and trailing control. */
@Composable
private fun NotificationSettingRow(
    title: String,
    detail: String,
    index: Int = 0,
    count: Int = 1,
    checked: Boolean? = null,
    selected: Boolean? = null,
    onClick: () -> Unit,
    control: @Composable () -> Unit,
) {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(if (pressed) 0.98f else 1f, spring(stiffness = 900f), label = "settingPress")
    val colors = MaterialTheme.colorScheme
    val typography = MaterialTheme.typography
    val action = when {
        checked != null -> Modifier.toggleable(
            value = checked, interactionSource = interaction, indication = null,
            role = Role.Switch, onValueChange = { onClick() },
        )
        selected != null -> Modifier.selectable(
            selected = selected, interactionSource = interaction, indication = null,
            role = Role.RadioButton, onClick = onClick,
        )
        else -> Modifier.clickable(
            interactionSource = interaction, indication = null, role = Role.Button, onClick = onClick,
        )
    }
    Row(
        Modifier.fillMaxWidth()
            .graphicsLayer { scaleX = scale; scaleY = scale }
            .clip(groupedRoundShape(index, count))
            .background(colors.surfaceContainerLow)
            .then(action)
            .padding(horizontal = 16.dp, vertical = 12.dp),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(3.dp)) {
            Text(title,
                style = avgSizeInStyle(typography.labelLargeEmphasized, typography.bodyLargeEmphasized, 0.75f),
                color = colors.onBackground)
            Text(detail, style = typography.bodyMedium, color = colors.onSurfaceVariant)
        }
        control()
    }
}

@Composable
private fun NotificationSettingsChevron() {
    MorphIcon(MorphGlyph.ChevronRight, null, Modifier.size(20.dp), tint = MaterialTheme.colorScheme.onSurfaceVariant)
}
