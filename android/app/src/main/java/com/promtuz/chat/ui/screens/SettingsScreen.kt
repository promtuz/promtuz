package com.promtuz.chat.ui.screens

import androidx.annotation.DrawableRes
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SettingsSection
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.listPadding

private data class SettingItem(
    val title: String,
    val summary: String,
    @param:DrawableRes val icon: Int,
    val route: NavKey,
)

private data class SettingGroup(val name: String, val items: List<SettingItem>)

@Composable
fun SettingsScreen(appViewModel: AppVM) {
    val groups = listOf(
        SettingGroup("Identity", listOf(
            SettingItem("Profile", "Your name, photo and bio", R.drawable.i_user, Routes.Profile),
            SettingItem(
                stringResource(R.string.identity_keys_title), "QR code and recovery phrase",
                R.drawable.i_key, Routes.IdentityKeys,
            ),
        )),
        SettingGroup("Chats", listOf(
            SettingItem("Privacy", "Who can message you, and blocked people", R.drawable.i_shield_lock, Routes.PrivacySettings),
            SettingItem(
                "Notifications", "Alerts and message previews",
                R.drawable.i_notifications, Routes.NotificationsSettings,
            ),
            SettingItem(
                "Chat appearance", "Theme, bubbles and wallpaper",
                R.drawable.i_chat_settings, Routes.ChatAppearance,
            ),
            SettingItem("Stickers", "Create and manage packs", R.drawable.i_sticker, Routes.Stickers),
        )),
        SettingGroup("Data & connection", listOf(
            SettingItem("Storage", "Media and space usage", R.drawable.i_usage_chart, Routes.Storage),
            SettingItem(
                "Backup & restore", "Save or restore an encrypted backup",
                R.drawable.i_chat_backup, Routes.BackupRestore,
            ),
            SettingItem("Relay nodes", "Connections and latency", R.drawable.i_network_nodes, Routes.Relays),
        )),
        SettingGroup("App", listOf(
            SettingItem("Updates", "App version and update channel", R.drawable.i_update, Routes.Updates),
            SettingItem("About Promtuz", "Share, source code and licenses", R.drawable.i_info, Routes.About),
        )),
    )

    SimpleScreen({ Text("Settings") }) { padding ->
        LazyColumn(
            modifier = Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            groups.forEachIndexed { groupIndex, group ->
                item(key = "heading-${group.name}", contentType = "heading") {
                    SettingsSection(group.name, top = if (groupIndex == 0) 0.dp else 20.dp)
                }
                itemsIndexed(group.items, key = { _, item -> item.route.toString() }, contentType = { _, _ -> "setting" }) { index, setting ->
                    GroupedActionRow(
                        title = setting.title,
                        index = index,
                        groupSize = group.items.size,
                        onClick = { appViewModel.navigator.push(setting.route) },
                        supportingText = setting.summary,
                    ) {
                        DrawableIcon(setting.icon, size = 26.dp)
                    }
                }
            }
        }
    }
}
