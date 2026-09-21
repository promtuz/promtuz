package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.text.avgSizeInStyle

private data class SettingItem(val title: String, val drawableIcon: Int, val onClick: () -> Unit)
private data class SettingGroup(val name: String, val items: List<SettingItem>)

@Composable
fun SettingsScreen(
    appViewModel: AppVM
) {
    val direction = LocalLayoutDirection.current
    val context = LocalContext.current
    val textTheme = MaterialTheme.typography
    val colors = MaterialTheme.colorScheme

    val navigate: (NavKey) -> Unit = { route -> appViewModel.navigator.push(route) }

    // @formatter:off
    val settingGroups = remember {
        listOf(
            SettingGroup(
                "General", listOf(
                    SettingItem(context.getString(R.string.identity_keys_title), R.drawable.i_key) { navigate(Routes.IdentityKeys) },
                    SettingItem("Privacy & Security", R.drawable.i_shield_lock) {},
                    // SettingItem("Blocked Users", R.drawable.i_user_blocked) {},
                    SettingItem(
                        "Storage", R.drawable.i_hard_drive
                    ) { navigate(Routes.Storage) },
                    SettingItem("Notifications", R.drawable.i_notifications) {
                        navigate(Routes.NotificationsSettings)
                    },
                )
            ),
            SettingGroup(
                "Appearance", listOf(
                    SettingItem(
                        "Chat Appearance", R.drawable.i_dark_mode
                    ) { navigate(Routes.ChatAppearance) },
                    SettingItem("Language", R.drawable.i_language) {},
                )
            ),
            SettingGroup(
                "Network", listOf(
                    SettingItem("Resolvers", R.drawable.i_dns) {},
                    SettingItem("Relay Nodes", R.drawable.i_hub) { navigate(Routes.Relays) },
                )
            ),
            SettingGroup(
                "Developer", listOf(
                    SettingItem("App Logs", R.drawable.i_logs) { navigate(Routes.Logs) },
                    SettingItem("Backup & Restore", R.drawable.i_encrypted) {
                        navigate(Routes.BackupRestore)
                    },
                )
            ),
            SettingGroup(
                "About", listOf(
                    SettingItem("App Info", R.drawable.i_info) { navigate(Routes.About) },
                    SettingItem("Updates", R.drawable.i_download) { navigate(Routes.Updates) },
                    SettingItem("Open Source Licenses", R.drawable.i_code) { navigate(Routes.OpenSourceLicenses) },
                )
            ),
        )
    }
    // @formatter:on

    SimpleScreen(
        { Text("Settings") },
    ) { padding ->
        LazyColumn(
            Modifier
                .fillMaxSize()
                .padding(
                    start = padding.calculateLeftPadding(direction),
                    end = padding.calculateRightPadding(direction),
                    top = 0.dp,
                    bottom = 0.dp
                ), contentPadding = PaddingValues(
                18.dp, padding.calculateTopPadding() + 12.dp, 18.dp, 48.dp
            ), verticalArrangement = Arrangement.spacedBy(4.dp)
        ) {
            for ((title, settings) in settingGroups) {
                item {
                    Text(
                        title.uppercase(),
                        Modifier.padding(top = 16.dp, bottom = 3.dp, start = 2.dp),
                        colors.onSurfaceVariant,
                        style = avgSizeInStyle(
                            textTheme.labelLargeEmphasized, textTheme.labelMediumEmphasized
                        )
                    )
                }
                itemsIndexed(settings) { index, setting ->
                    GroupedActionRow(setting.title, index, settings.size, setting.onClick) {
                        Icon(painterResource(setting.drawableIcon), null, Modifier.size(26.dp))
                    }
                }
            }
        }
    }

}
