package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.ui.components.TextButton
import android.app.NotificationManager
import android.content.Intent
import android.media.RingtoneManager
import android.net.Uri
import android.provider.Settings
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.content.IntentCompat
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LifecycleEventEffect
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.navigation.AppNavigator
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.utils.extensions.toHex
import com.promtuz.core.CoreBridge
import com.promtuz.core.push.AlertKind
import com.promtuz.core.push.AlertStyle
import com.promtuz.core.push.Notifications
import com.promtuz.core.push.Vibe

private data class MutedChat(val hex: String, val name: String, val identity: String)

/** Muted chats of each kind, which never notify. */
private suspend fun mutedChats(): Map<Boolean, List<MutedChat>> = runCatching {
    val contacts = CoreBridge.contacts().associate { it.ipk.toHex() to it.name }
    CoreBridge.listConversations().filter { it.muted && !it.request }.map { c ->
        val group = c.kind.toInt() == 1
        val peer = c.peer?.toHex()
        group to MutedChat(
            c.id.toHex(),
            (if (group) c.displayName else contacts[peer].orEmpty()).ifBlank { "Unknown" },
            if (group) "group:${c.id.toHex()}" else peer ?: c.id.toHex(),
        )
    }.groupBy({ it.first }, { it.second })
}.getOrDefault(emptyMap())

private fun kindSubtitle(on: Boolean, style: AlertStyle, muted: Int, ctx: android.content.Context) =
    if (!on) "Off" else listOfNotNull(style.soundLabel(ctx).let { if (it == "Default") "Default sound" else it },
        muted.takeIf { it > 0 }?.let { "$it muted" }).joinToString(" · ")

@Composable
fun NotificationsSettingsScreen(nav: AppNavigator) {
    val context = LocalContext.current
    val manager = remember(context) { context.getSystemService(NotificationManager::class.java) }
    remember { ChatPrefs.foldNotifSwitch() }
    var chats by remember { mutableStateOf(ChatPrefs.notifChats) }
    var groups by remember { mutableStateOf(ChatPrefs.notifGroups) }
    var requests by remember { mutableStateOf(ChatPrefs.notifRequests) }
    var preview by remember { mutableStateOf(ChatPrefs.notifPreview) }
    var banners by remember { mutableStateOf(ChatPrefs.inAppBanners) }
    var sounds by remember { mutableStateOf(ChatPrefs.inAppSounds) }
    var vibrate by remember { mutableStateOf(ChatPrefs.inAppVibrate) }
    var openChat by remember { mutableStateOf(ChatPrefs.openChatSounds) }
    var badgeChats by remember { mutableStateOf(ChatPrefs.badgeChats) }
    var styles by remember { mutableStateOf(AlertKind.entries.associateWith { Notifications.style(context, it) }) }
    var systemEnabled by remember { mutableStateOf(manager.areNotificationsEnabled()) }
    var channelsBlocked by remember { mutableStateOf(Notifications.messagesBlocked(context)) }
    var muted by remember { mutableStateOf<Map<Boolean, List<MutedChat>>>(emptyMap()) }
    var confirmReset by remember { mutableStateOf(false) }

    fun reload() {
        chats = ChatPrefs.notifChats; groups = ChatPrefs.notifGroups
        requests = ChatPrefs.notifRequests; preview = ChatPrefs.notifPreview; banners = ChatPrefs.inAppBanners
        sounds = ChatPrefs.inAppSounds; vibrate = ChatPrefs.inAppVibrate; openChat = ChatPrefs.openChatSounds
        badgeChats = ChatPrefs.badgeChats
        styles = AlertKind.entries.associateWith { Notifications.style(context, it) }
    }

    // Android owns the permission and channels, and the kind pages edit the rest; reread on return.
    LifecycleEventEffect(Lifecycle.Event.ON_RESUME) {
        systemEnabled = manager.areNotificationsEnabled()
        channelsBlocked = Notifications.messagesBlocked(context)
        reload()
    }
    LaunchedEffect(Unit) {
        muted = mutedChats()
    }
    val appSettings = {
        context.startActivity(Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE, context.packageName))
    }

    GroupedScreen({ Text("Notifications") }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            when {
                !systemEnabled -> WarningBanner(
                    R.drawable.i_notifications, "Android is blocking notifications",
                    "You won’t hear about messages or calls until you allow them.", "Allow notifications", appSettings,
                )
                channelsBlocked -> WarningBanner(
                    R.drawable.i_notifications, "Message alerts are off in Android",
                    "New messages arrive silently until you turn them back on.", "Turn on", appSettings,
                )
                else -> null
            }?.let { CardGap() }

            SettingsCard(header = "Notify me about") {
                SettingsSwitchRow(
                    "Chats", chats, { chats = it; ChatPrefs.notifChats = it },
                    subtitle = kindSubtitle(chats, styles.getValue(AlertKind.Chats), muted[false].orEmpty().size, context),
                    icon = R.drawable.i_user,
                    onClick = { nav.push(Routes.NotificationsKind(groups = false)) },
                )
                SettingsSwitchRow(
                    "Groups", groups, { groups = it; ChatPrefs.notifGroups = it },
                    subtitle = kindSubtitle(groups, styles.getValue(AlertKind.Groups), muted[true].orEmpty().size, context),
                    icon = R.drawable.i_users,
                    onClick = { nav.push(Routes.NotificationsKind(groups = true)) },
                )
                SettingsSwitchRow(
                    "Message requests", requests, { requests = it; ChatPrefs.notifRequests = it },
                    subtitle = "From people you haven’t added",
                    icon = R.drawable.i_comment_plus,
                )
            }
            CardGap()

            SettingsCard(header = "On your lock screen", footer = "Locked chats never notify you.") {
                SettingsSwitchRow(
                    "Show previews", preview, { preview = it; ChatPrefs.notifPreview = it },
                    subtitle = if (preview) "Names and message text" else "Only “New message”",
                )
            }
            CardGap()

            SettingsCard(header = "While you’re in Promtuz") {
                SettingsSwitchRow("Sounds", sounds, { sounds = it; ChatPrefs.inAppSounds = it })
                SettingsSwitchRow("Vibration", vibrate, { vibrate = it; ChatPrefs.inAppVibrate = it })
                SettingsSwitchRow(
                    "Message banners", banners, { banners = it; ChatPrefs.inAppBanners = it },
                    subtitle = "A preview at the top when you’re in another chat",
                )
                SettingsSwitchRow("Sounds in the open chat", openChat, { openChat = it; ChatPrefs.openChatSounds = it })
            }
            CardGap()

            SettingsCard(header = "App icon badge") {
                SettingsChoiceRow(
                    "Count",
                    choices = listOf(
                        Choice(false, "Unread messages", "Every unread message adds one"),
                        Choice(true, "Unread chats", "A chat counts once, however many messages wait"),
                    ),
                    selected = badgeChats,
                    onSelect = { badgeChats = it; ChatPrefs.badgeChats = it },
                )
            }
            CardGap()


            SettingsCard {
                SettingsRow("Reset notification settings", titleColor = Promtuz.colors.danger, onClick = { confirmReset = true })
            }
        }
    }

    if (confirmReset) AppAlertDialog(
        onDismissRequest = { confirmReset = false },
        title = { Text("Reset notifications?") },
        text = { Text("Sounds, vibration and every switch on this page go back to how they started. Muted chats stay muted.") },
        confirmButton = {
            TextButton(onClick = {
                ChatPrefs.resetNotifications()
                Notifications.reset(context)
                reload()
                confirmReset = false
            }) { Text("Reset", color = MaterialTheme.colorScheme.error) }
        },
        dismissButton = { TextButton(onClick = { confirmReset = false }) { Text("Cancel") } },
    )
}

@Composable
fun NotificationsKindScreen(groups: Boolean, openChat: (hex: String, name: String) -> Unit) {
    val context = LocalContext.current
    val kind = if (groups) AlertKind.Groups else AlertKind.Chats
    var on by remember { ChatPrefs.foldNotifSwitch(); mutableStateOf(if (groups) ChatPrefs.notifGroups else ChatPrefs.notifChats) }
    var style by remember { mutableStateOf(Notifications.style(context, kind)) }
    var muted by remember { mutableStateOf<List<MutedChat>>(emptyList()) }
    LaunchedEffect(Unit) { muted = mutedChats()[groups].orEmpty() }

    fun update(s: AlertStyle) { style = s; Notifications.setStyle(context, kind, s) }

    val picker = rememberLauncherForActivityResult(ActivityResultContracts.StartActivityForResult()) { result ->
        if (result.resultCode != android.app.Activity.RESULT_OK) return@rememberLauncherForActivityResult
        val uri = result.data?.let { IntentCompat.getParcelableExtra(it, RingtoneManager.EXTRA_RINGTONE_PICKED_URI, Uri::class.java) }
        update(style.copy(sound = when (uri) {
            null -> "none"
            Settings.System.DEFAULT_NOTIFICATION_URI -> ""
            else -> uri.toString()
        }))
    }

    GroupedScreen({ Text(kind.label) }) { padding ->
        Column(
            Modifier.fillMaxSize().verticalScroll(rememberScrollState()).padding(padding.listPadding(top = 6.dp, bottom = 24.dp)),
        ) {
            SettingsCard {
                SettingsSwitchRow("Notify me about ${kind.label.lowercase()}", on, {
                    on = it
                    if (groups) ChatPrefs.notifGroups = it else ChatPrefs.notifChats = it
                })
            }
            CardGap()

            SettingsCard(header = "Alerts") {
                SettingsRow("Sound", value = style.soundLabel(context), valueColor = Promtuz.colors.accent, enabled = on, onClick = {
                    picker.launch(Intent(RingtoneManager.ACTION_RINGTONE_PICKER)
                        .putExtra(RingtoneManager.EXTRA_RINGTONE_TYPE, RingtoneManager.TYPE_NOTIFICATION)
                        .putExtra(RingtoneManager.EXTRA_RINGTONE_TITLE, "${kind.label} sound")
                        .putExtra(RingtoneManager.EXTRA_RINGTONE_SHOW_DEFAULT, true)
                        .putExtra(RingtoneManager.EXTRA_RINGTONE_SHOW_SILENT, true)
                        .putExtra(RingtoneManager.EXTRA_RINGTONE_EXISTING_URI, style.soundUri))
                })
                SettingsChoiceRow(
                    "Vibration",
                    choices = Vibe.entries.map { Choice(it, it.label) },
                    selected = style.vibe,
                    onSelect = { update(style.copy(vibe = it)) },
                    enabled = on,
                )
                SettingsSwitchRow(
                    "Pop up on screen", style.popup, { update(style.copy(popup = it)) },
                    subtitle = "Slides in at the top while you use your phone",
                    enabled = on,
                )
            }

            if (muted.isNotEmpty()) {
                CardGap()
                SettingsCard(header = "Muted", footer = "Muted ${kind.label.lowercase()} never notify you.") {
                    muted.forEach { chat ->
                        SettingsRow(
                            chat.name,
                            leading = { Avatar(chat.name, size = 40.dp, identityKey = chat.identity) },
                            leadingSize = 40.dp,
                            onClick = { openChat(chat.hex, chat.name) },
                        )
                    }
                }
            }
        }
    }
}

/** A warm card for a state that stops the feature working, with the one action that fixes it. */
@Composable
private fun WarningBanner(icon: Int, title: String, body: String, action: String, onAction: () -> Unit) {
    val c = Promtuz.colors
    Row(
        Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.warnBg).padding(16.dp),
        horizontalArrangement = Arrangement.spacedBy(14.dp),
    ) {
        DrawableIcon(icon, size = 22.dp, tint = c.warn)
        Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(title, style = TextStyle(fontSize = 16.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium), color = c.text)
            Text(body, style = RowSubtitleStyle, color = c.muted)
            Text(
                action,
                Modifier.padding(top = 6.dp).clip(RoundedCornerShape(8.dp)).clickable(role = Role.Button, onClick = onAction)
                    .padding(vertical = 4.dp),
                style = TextStyle(fontSize = 15.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium),
                color = c.warn,
            )
        }
    }
}
