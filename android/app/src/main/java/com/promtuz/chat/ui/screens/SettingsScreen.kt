package com.promtuz.chat.ui.screens

import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.unit.dp
import androidx.navigation3.runtime.NavKey
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.ProfileWork
import com.promtuz.chat.presentation.viewmodel.SettingsVM
import com.promtuz.chat.ui.components.Avatar
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.text.avgSizeInStyle
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
            item { ProfileHeader(viewModel) }
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

/**
 * Who we are, at the top of settings: our picture and name. Tapping the picture
 * offers to choose or remove it. The picture is what every contact sees, so the
 * header shows what core stored after a change, never the raw pick.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun ProfileHeader(viewModel: SettingsVM) {
    val profile by viewModel.profile.collectAsState()
    val work by viewModel.work.collectAsState()
    val busy = work is ProfileWork.Busy
    var menu by remember { mutableStateOf(false) }
    val pick = rememberLauncherForActivityResult(ActivityResultContracts.PickVisualMedia()) { uri ->
        if (uri != null) viewModel.setPicture(uri)
    }
    val editLabel = stringResource(R.string.profile_photo_edit)

    Column(
        Modifier.fillMaxWidth().padding(top = 8.dp, bottom = 12.dp),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        Box(Modifier.semantics { contentDescription = editLabel }) {
            // A plain tile, like every other avatar in the app; the tap is the
            // affordance, and a save in flight just ignores the next one.
            Avatar(
                profile.name, size = 96.dp, image = profile.picture,
                onClick = { if (!busy) menu = true },
            )
            DropdownMenu(expanded = menu, onDismissRequest = { menu = false }) {
                DropdownMenuItem(
                    text = { Text(stringResource(R.string.profile_photo_choose)) },
                    leadingIcon = { Icon(painterResource(R.drawable.oi_image), null) },
                    onClick = {
                        menu = false
                        pick.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageOnly))
                    },
                )
                if (profile.picture != null) DropdownMenuItem(
                    text = { Text(stringResource(R.string.profile_photo_remove)) },
                    leadingIcon = { Icon(painterResource(R.drawable.oi_trash), null) },
                    onClick = { menu = false; viewModel.removePicture() },
                )
            }
        }
        Text(profile.name, style = MaterialTheme.typography.titleLargeEmphasized)
        (work as? ProfileWork.Failed)?.let {
            Text(it.reason, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
        }
    }
}
