package com.promtuz.chat.ui.screens

import androidx.annotation.DrawableRes
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.text.avgSizeInStyle

private enum class SettingsIcon(@param:DrawableRes val drawable: Int) {
    Profile(R.drawable.i_user),
    Identity(R.drawable.i_key),
    Notifications(R.drawable.i_notifications),
    Appearance(R.drawable.i_chat_settings),
    Stickers(R.drawable.i_sticker),
    Storage(R.drawable.i_usage_chart),
    Backup(R.drawable.i_chat_backup),
    Relays(R.drawable.i_network_nodes),
    Updates(R.drawable.i_update),
    About(R.drawable.i_info),
}

private data class SettingItem(
    val title: String,
    val summary: String,
    val icon: SettingsIcon,
    val route: NavKey,
)

private data class SettingGroup(val name: String, val items: List<SettingItem>)

@Composable
fun SettingsScreen(appViewModel: AppVM) {
    val direction = LocalLayoutDirection.current
    val textTheme = MaterialTheme.typography
    val colors = MaterialTheme.colorScheme
    val groups = listOf(
        SettingGroup("Identity", listOf(
            SettingItem("Profile", "Your name, photo and bio", SettingsIcon.Profile, Routes.Profile),
            SettingItem(
                stringResource(R.string.identity_keys_title), "QR code and recovery phrase",
                SettingsIcon.Identity, Routes.IdentityKeys,
            ),
        )),
        SettingGroup("Chats", listOf(
            SettingItem(
                "Notifications", "Alerts and message previews",
                SettingsIcon.Notifications, Routes.NotificationsSettings,
            ),
            SettingItem(
                "Chat appearance", "Theme, bubbles and wallpaper",
                SettingsIcon.Appearance, Routes.ChatAppearance,
            ),
            SettingItem("Stickers", "Create and manage packs", SettingsIcon.Stickers, Routes.Stickers),
        )),
        SettingGroup("Data & connection", listOf(
            SettingItem("Storage", "Media and space usage", SettingsIcon.Storage, Routes.Storage),
            SettingItem(
                "Backup & restore", "Save or restore an encrypted backup",
                SettingsIcon.Backup, Routes.BackupRestore,
            ),
            SettingItem("Relay nodes", "Connections and latency", SettingsIcon.Relays, Routes.Relays),
        )),
        SettingGroup("App", listOf(
            SettingItem("Updates", "App version and update channel", SettingsIcon.Updates, Routes.Updates),
            SettingItem("About Promtuz", "Share, source code and licenses", SettingsIcon.About, Routes.About),
        )),
    )

    SimpleScreen({ Text("Settings") }) { padding ->
        LazyColumn(
            modifier = Modifier.fillMaxSize().padding(
                start = padding.calculateLeftPadding(direction),
                end = padding.calculateRightPadding(direction),
            ),
            contentPadding = PaddingValues(
                start = 18.dp,
                top = padding.calculateTopPadding() + 12.dp,
                end = 18.dp,
                bottom = padding.calculateBottomPadding() + 24.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            groups.forEachIndexed { groupIndex, group ->
                item(key = "heading-${group.name}", contentType = "heading") {
                    Text(
                        group.name.uppercase(),
                        Modifier.padding(
                            top = if (groupIndex == 0) 0.dp else 20.dp,
                            bottom = 3.dp,
                            start = 2.dp,
                        ).semantics { heading() },
                        colors.onSurfaceVariant,
                        style = avgSizeInStyle(textTheme.labelLargeEmphasized, textTheme.labelMediumEmphasized),
                    )
                }
                itemsIndexed(group.items, key = { _, item -> item.icon }, contentType = { _, _ -> "setting" }) { index, setting ->
                    GroupedActionRow(
                        title = setting.title,
                        index = index,
                        groupSize = group.items.size,
                        onClick = { appViewModel.navigator.push(setting.route) },
                        supportingText = setting.summary,
                    ) {
                        DrawableIcon(setting.icon.drawable, size = 26.dp)
                    }
                }
            }
        }
    }
}
