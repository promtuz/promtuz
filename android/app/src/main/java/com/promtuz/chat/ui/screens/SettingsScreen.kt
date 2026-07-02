package com.promtuz.chat.ui.screens

import androidx.compose.foundation.background
import androidx.compose.foundation.combinedClickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.navigation.goTo
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.SettingsVM
import com.promtuz.chat.ui.activities.ManageSpace
import com.promtuz.chat.ui.components.FlexibleScreen
import com.promtuz.chat.ui.text.avgSizeInStyle
import com.promtuz.chat.ui.util.groupedRoundShape
import org.koin.androidx.compose.koinViewModel

private data class SettingItem(val title: String, val drawableIcon: Int, val onClick: () -> Unit)
private data class SettingGroup(val name: String, val items: List<SettingItem>)

@Composable
fun SettingsScreen(
    appViewModel: AppVM, viewModel: SettingsVM = koinViewModel()
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
                    SettingItem("Identity & Keys", R.drawable.i_key) {},
                    SettingItem("Privacy & Security", R.drawable.i_shield_lock) {},
                    SettingItem("Blocked Users", R.drawable.i_user_blocked) {},
                    SettingItem(
                        "Storage", R.drawable.i_hard_drive
                    ) { context.goTo(ManageSpace::class.java) },
                    SettingItem("Notifications", R.drawable.i_notifications) {},
                )
            ),
            SettingGroup(
                "Appearance", listOf(
                    SettingItem("Theme", R.drawable.i_dark_mode) {},
                    SettingItem("Font Size", R.drawable.i_format_size) {},
                    SettingItem("Language", R.drawable.i_language) {},
                )
            ),
            SettingGroup(
                "Network", listOf(
                    SettingItem("Statistics", R.drawable.i_connection_status) { navigate(Routes.NetworkStats) },
                    SettingItem("Resolvers", R.drawable.i_dns) {},
                    SettingItem("Relay Nodes", R.drawable.i_hub) {},
                )
            ),
            SettingGroup(
                "Developer", listOf(
                    SettingItem("App Logs", R.drawable.i_logs) { navigate(Routes.Logs) },
                )
            ),
            SettingGroup(
                "About", listOf(
                    SettingItem("App Info", R.drawable.i_info) { navigate(Routes.About) },
                    SettingItem("Open Source Licenses", R.drawable.i_code) { },
                )
            ),
        )
    }
    // @formatter:on

    FlexibleScreen(
        { Text("Settings") },
    ) { padding, scrollBehavior ->
        LazyColumn(
            Modifier
                .fillMaxSize()
                .nestedScroll(scrollBehavior.nestedScrollConnection)
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
                    SettingsGroup(Modifier, setting, index to settings.size)
                }
            }
        }
    }
}


@Composable
private fun SettingsGroup(
    modifier: Modifier = Modifier, setting: SettingItem, groupEntry: Pair<Int, Int>
) {
    val (index, groupSize) = groupEntry

    val colors = MaterialTheme.colorScheme
    val textTheme = MaterialTheme.typography

    Row(
        modifier
            .fillMaxWidth()
            .clip(groupedRoundShape(index, groupSize))
            .background(colors.surfaceContainerLow)
            .combinedClickable(
                onClick = {
                    setting.onClick.invoke()
                },
            )
            .padding(vertical = 12.dp, horizontal = 16.dp),
        horizontalArrangement = Arrangement.spacedBy(20.dp),
        verticalAlignment = Alignment.CenterVertically
    ) {
        Icon(
            painterResource(setting.drawableIcon),
            setting.title,
            Modifier.size(26.dp),
            tint = colors.onSurface
        )
        Text(
            setting.title, style = avgSizeInStyle(
                textTheme.labelLargeEmphasized, textTheme.bodyLargeEmphasized, 0.75f
            ), color = colors.onBackground
        )
    }
}