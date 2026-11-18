package com.promtuz.chat.ui.screens

import android.content.ActivityNotFoundException
import android.content.Intent
import android.text.format.DateUtils
import androidx.annotation.DrawableRes
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.LazyListScope
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.core.net.toUri
import androidx.lifecycle.compose.LifecycleResumeEffect
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.data.ChatPrefs
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.BackupRestoreVM
import com.promtuz.chat.presentation.viewmodel.ProfileVM
import com.promtuz.chat.presentation.viewmodel.StickersVM
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.appearance.AppearanceStore
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.CardGap
import com.promtuz.chat.ui.components.SettingsCard
import com.promtuz.chat.ui.components.SettingsRow
import com.promtuz.chat.ui.components.GroupedScreen
import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.update.offered
import com.promtuz.core.CoreBridge
import org.koin.androidx.compose.koinViewModel

private class SettingRow(
    val title: String,
    @param:DrawableRes val icon: Int,
    val value: String? = null,
    val valueColor: Color? = null,
    val onClick: () -> Unit,
)

@Composable
fun SettingsScreen(
    appViewModel: AppVM,
    profileVM: ProfileVM,
    updates: UpdateVM = koinViewModel(),
    stickers: StickersVM = koinViewModel(),
    backup: BackupRestoreVM = koinViewModel(),
) {
    val context = LocalContext.current
    val c = Promtuz.colors
    val nav = appViewModel.navigator
    val profile by profileVM.profile.collectAsState()
    val appearance by AppearanceStore.appearance.collectAsState()
    val update by updates.state.collectAsState()
    val channel by updates.channel.collectAsState()
    val packs by stickers.packs.collectAsStateWithLifecycle()
    val lastBackup by backup.lastBackup.collectAsState()

    // Values owned by other screens or by core have no stream; reread them each time Settings shows.
    var notifications by remember { mutableStateOf(true) }
    var everyone by remember { mutableStateOf<Boolean?>(null) }
    var relaysUp by remember { mutableStateOf<Int?>(null) }
    LifecycleResumeEffect(Unit) {
        notifications = ChatPrefs.notifEnabled && (ChatPrefs.notifChats || ChatPrefs.notifGroups || ChatPrefs.notifRequests)
        onPauseOrDispose {}
    }
    LaunchedEffect(Unit) {
        profileVM.refresh()
        everyone = runCatching { CoreBridge.messageRequestsEnabled() }.getOrNull()
        relaysUp = runCatching { CoreBridge.relays().count { it.isConnected } }.getOrNull()
    }

    fun open(intent: Intent) = try { context.startActivity(intent) } catch (_: ActivityNotFoundException) {}
    fun go(route: NavKey) = { nav.push(route) }

    val groups = listOf(
        listOf(
            SettingRow("Notifications", R.drawable.i_notifications, if (notifications) "On" else "Off", onClick = go(Routes.NotificationsSettings)),
            SettingRow("Privacy and security", R.drawable.i_shield_lock, everyone?.let { if (it) "Everyone" else "My contacts" }, onClick = go(Routes.PrivacySettings)),
            SettingRow("Appearance", R.drawable.i_chat_settings, appearance.themeMode.name, onClick = go(Routes.ChatAppearance)),
            SettingRow("Stickers", R.drawable.i_sticker, packs.size.takeIf { it > 0 }?.let { if (it == 1) "1 pack" else "$it packs" }, onClick = go(Routes.Stickers)),
        ),
        listOf(
            SettingRow("Recovery phrase", R.drawable.i_key, onClick = go(Routes.RecoveryPhrase)),
            SettingRow("Backup", R.drawable.i_chat_backup, lastBackup?.let { relativeDay(it) } ?: "Never", onClick = go(Routes.BackupRestore)),
            SettingRow("Storage", R.drawable.i_usage_chart, onClick = go(Routes.Storage)),
        ),
        listOf(
            SettingRow("Relays", R.drawable.i_network_nodes, relaysUp?.let { "$it connected" }, onClick = go(Routes.Relays)),
            SettingRow("Updates", R.drawable.i_update, update.offered?.let { "${it.versionName} available" },
                valueColor = c.accent, onClick = go(Routes.Updates)),
        ),
        listOf(
            SettingRow("Invite a friend", R.drawable.i_user_add) {
                val link = "https://apt.promtuz.dev/apk/$channel/arm64-v8a/latest.apk"
                open(Intent.createChooser(
                    Intent(Intent.ACTION_SEND).setType("text/plain")
                        .putExtra(Intent.EXTRA_SUBJECT, "Promtuz for Android")
                        .putExtra(Intent.EXTRA_TEXT, "Let's talk on Promtuz\n$link"),
                    "Invite a friend",
                ))
            },
            SettingRow("Help and feedback", R.drawable.i_help) {
                open(Intent(Intent.ACTION_VIEW, "https://github.com/promtuz/promtuz/issues".toUri()))
            },
            SettingRow("About Promtuz", R.drawable.i_logo_mono, onClick = go(Routes.About)),
        ),
    )

    GroupedScreen({ Text("Settings") }) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(top = 6.dp, bottom = 24.dp)) {
            item("profile") {
                ProfileCard(
                    name = profile.name,
                    bio = profile.bio,
                    identity = profile.identity,
                    picture = profile.picture,
                    onOpen = go(Routes.Profile),
                    onQr = go(Routes.ShareIdentity),
                )
            }
            groups.forEachIndexed { g, rows -> settingCard(g, rows) }
            item("footer") {
                Text(
                    "Promtuz ${BuildConfig.VERSION_NAME} · ${updates.installedChannel.replaceFirstChar { it.uppercase() }} channel",
                    Modifier.fillMaxWidth().padding(top = 18.dp),
                    style = TextStyle(fontSize = 13.sp, lineHeight = 18.sp),
                    color = c.muted,
                    textAlign = TextAlign.Center,
                )
            }
        }
    }
}

private fun LazyListScope.settingCard(group: Int, rows: List<SettingRow>) {
    item("card-$group", contentType = "setting") {
        Column {
            CardGap()
            SettingsCard {
                rows.forEach { row ->
                    SettingsRow(row.title, icon = row.icon, value = row.value, valueColor = row.valueColor, onClick = row.onClick)
                }
            }
        }
    }
}

@Composable
private fun ProfileCard(
    name: String,
    bio: String,
    identity: String,
    picture: com.promtuz.chat.utils.media.AvatarPicture?,
    onOpen: () -> Unit,
    onQr: () -> Unit,
) {
    val c = Promtuz.colors
    val pad = 16.dp
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(Radii.card))
            .background(c.card)
            .clickable(role = Role.Button, onClick = onOpen)
            .padding(pad),
        horizontalArrangement = Arrangement.spacedBy(16.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Avatar(name.ifBlank { "?" }, size = 64.dp, identityKey = identity.ifBlank { name }, image = picture)
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text(name, style = TextStyle(fontSize = 20.sp, lineHeight = 26.sp, fontWeight = FontWeight.Medium),
                color = c.text, maxLines = 1, overflow = TextOverflow.Ellipsis)
            Text(bio.ifBlank { "Add a bio" }, style = TextStyle(fontSize = 14.sp, lineHeight = 20.sp),
                color = c.muted, maxLines = 1, overflow = TextOverflow.Ellipsis)
        }
        Box(
            Modifier.size(48.dp).clip(RoundedCornerShape(Radii.iconButton)).background(c.cardHigh)
                .clickable(role = Role.Button, onClickLabel = "My QR code", onClick = onQr),
            contentAlignment = Alignment.Center,
        ) {
            DrawableIcon(R.drawable.i_qr_code, desc = "My QR code", size = 24.dp, tint = c.accent)
        }
    }
}

private fun relativeDay(ms: Long): String =
    DateUtils.getRelativeTimeSpanString(ms, System.currentTimeMillis(), DateUtils.DAY_IN_MILLIS).toString()
