package com.promtuz.chat.ui.screens

import android.content.Context
import android.text.format.Formatter
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.ui.components.AppAlertDialog
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.Button
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.MorphIcon
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.releaseNotes
import com.promtuz.chat.ui.components.installedReleaseNotes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.navigation.Routes
import org.koin.compose.koinInject
import com.promtuz.chat.update.offered
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.update.UpdateManifest
import com.promtuz.chat.update.UpdateState
import org.koin.androidx.compose.koinViewModel

@Composable
fun UpdateScreen(viewModel: UpdateVM = koinViewModel()) {
    val state by viewModel.state.collectAsState()
    val channel by viewModel.channel.collectAsState()
    val notes by viewModel.notes.collectAsState()
    val installedChanges by viewModel.installedChanges.collectAsState()
    val app = koinInject<AppVM>()
    val context = LocalContext.current
    var pendingChannel by remember { mutableStateOf<String?>(null) }

    LifecycleResumeEffect(viewModel) {
        viewModel.setScreenVisible(true)
        viewModel.check()
        onPauseOrDispose { viewModel.setScreenVisible(false) }
    }

    val channelActions = listOf("release", "debug").map { option ->
        MenuAction(
            "${option.replaceFirstChar { it.uppercase() }} channel",
            glyph = if (channel == option) MorphGlyph.Check else null,
        ) {
            if (option != channel) {
                when (state) {
                    is UpdateState.Downloading, is UpdateState.Ready -> pendingChannel = option
                    else -> viewModel.switchChannel(option)
                }
            }
        }
    }

    SimpleScreen(
        { Text("Updates") },
        actions = {
            AppDropMenu(
                anchor = {
                    DrawableIcon(R.drawable.i_more_vert, Modifier.padding(12.dp), desc = "Update options")
                },
                groups = listOf(listOf(
                    MenuAction("Check for updates") { viewModel.check() },
                    MenuAction("Release history") { app.navigator.push(Routes.ReleaseHistory) },
                )),
            )
        },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(24.dp, 16.dp, 24.dp),
            verticalArrangement = Arrangement.spacedBy(24.dp),
        ) {
            item("app") {
                Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(16.dp)) {
                    Image(painterResource(R.drawable.logo_colored), null, Modifier.size(48.dp))
                    Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text("Promtuz", style = MaterialTheme.typography.titleMedium)
                        UpdateDetail("${BuildConfig.VERSION_NAME} · ${viewModel.installedChannel.replaceFirstChar { it.uppercase() }}")
                    }
                    AppDropMenu(
                        anchor = {
                            Row(Modifier.padding(vertical = 12.dp),
                                verticalAlignment = Alignment.CenterVertically,
                                horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                Text(channel.replaceFirstChar { it.uppercase() },
                                    style = MaterialTheme.typography.labelLarge,
                                    color = MaterialTheme.colorScheme.primary)
                                MorphIcon(MorphGlyph.ChevronDown, description = null,
                                    modifier = Modifier.size(14.dp), tint = MaterialTheme.colorScheme.primary)
                            }
                        },
                        groups = listOf(channelActions), onClickLabel = "Change update channel",
                    )
                }
            }
            item("status-divider") {
                HorizontalDivider(color = MaterialTheme.colorScheme.outlineVariant)
            }
            item("status") {
                AnimatedContent(
                    targetState = state,
                    contentKey = { it::class },
                    contentAlignment = Alignment.TopStart,
                    transitionSpec = {
                        (fadeIn(Tweens.microInteraction(220)) + slideInVertically(Tweens.microInteraction(220)) { it / 5 }) togetherWith
                            (fadeOut(Tweens.microInteraction(160)) + slideOutVertically(Tweens.microInteraction(160)) { -it / 5 })
                    },
                    label = "updateStatus",
                ) { shown ->
                    Column(
                        Modifier.fillMaxWidth(),
                        horizontalAlignment = Alignment.Start,
                        verticalArrangement = Arrangement.spacedBy(16.dp),
                    ) {
                        val active = state::class == shown::class
                        val manifest = shown.offered
                        val switching = manifest?.versionCode == BuildConfig.VERSION_CODE && channel != viewModel.installedChannel
                        UpdateTitle(when (shown) {
                            UpdateState.Unchecked -> "Check for updates"
                            UpdateState.None -> "You're up to date"
                            UpdateState.Checking -> "Checking for updates"
                            is UpdateState.Available -> if (switching) "Switch to $channel" else "Update available"
                            is UpdateState.Downloading -> "Downloading update"
                            is UpdateState.Ready -> "Ready to install"
                            is UpdateState.Error -> "Couldn't update Promtuz"
                        })
                        manifest?.let { UpdateDetail(versionLine(context, it)) }
                        when (shown) {
                            UpdateState.Unchecked -> UpdateCheckAction("Check now", active, viewModel::check)
                            UpdateState.None -> UpdateCheckAction("Check again", active, viewModel::check)
                            UpdateState.Checking -> LinearProgressIndicator(Modifier.fillMaxWidth().padding(vertical = 12.dp))
                            is UpdateState.Available -> UpdateAction(
                                if (switching) "Download $channel build" else "Download", active,
                            ) { viewModel.download(shown.manifest) }
                            is UpdateState.Downloading -> {
                                LinearProgressIndicator({ shown.progress }, Modifier.fillMaxWidth())
                                UpdateDetail("${(shown.progress * 100).toInt()}% · ${Formatter.formatShortFileSize(context, shown.manifest.size)}")
                                TextButton(viewModel::cancelDownload, enabled = active) { Text("Cancel download") }
                            }
                            is UpdateState.Ready -> {
                                UpdateDetail("Confirm the update in Android's installer.")
                                UpdateAction("Install", active) { viewModel.install(shown.manifest, shown.apk) }
                            }
                            is UpdateState.Error -> {
                                UpdateDetail(shown.message)
                                UpdateCheckAction("Try again", active, viewModel::check)
                            }
                        }
                    }
                }
            }
            state.offered?.let {
                val switching = it.versionCode == BuildConfig.VERSION_CODE && channel != viewModel.installedChannel
                releaseNotes(notes, it.versionName,
                    heading = if (switching) "About this release" else "Coming in this update",
                    subtitle = when {
                        switching -> "$channel · ${it.versionName}"
                        it.versionName == BuildConfig.VERSION_NAME -> "${it.versionName} · Build ${BuildConfig.VERSION_CODE} → ${it.versionCode}"
                        else -> "${BuildConfig.VERSION_NAME} → ${it.versionName}"
                    },
                    combined = true, separator = true)
            }
            installedReleaseNotes(installedChanges)

        }
    }

    UpdateInstallPermissionDialog(state, viewModel)

    pendingChannel?.let { selected ->
        AppAlertDialog(
            onDismissRequest = { pendingChannel = null },
            title = { Text("Switch to ${selected.replaceFirstChar { it.uppercase() }}?") },
            text = { Text("The current update download will be discarded.") },
            confirmButton = {
                TextButton({ pendingChannel = null; viewModel.switchChannel(selected) }) { Text("Switch") }
            },
            dismissButton = { TextButton({ pendingChannel = null }) { Text("Cancel") } },
        )
    }
}

@Composable
internal fun UpdateInstallPermissionDialog(state: UpdateState, viewModel: UpdateVM) {
    if (state !is UpdateState.Ready || !state.needsInstallPermission) return
    AppAlertDialog(
        onDismissRequest = viewModel::dismissInstallPermission,
        title = { Text("Allow updates") },
        text = { Text("Android needs your permission to install updates from Promtuz. You can allow this in system settings.") },
        confirmButton = { TextButton(viewModel::requestInstallPermission) { Text("Settings") } },
        dismissButton = { TextButton(viewModel::dismissInstallPermission) { Text("Not now") } },
    )
}

@Composable
private fun UpdateCheckAction(label: String, enabled: Boolean, onClick: () -> Unit) {
    OutlinedButton(onClick, enabled = enabled) { Text(label) }
}

@Composable
private fun UpdateAction(label: String, enabled: Boolean, onClick: () -> Unit) {
    Button(onClick, Modifier.fillMaxWidth().heightIn(min = 52.dp), enabled = enabled) { Text(label) }
}

@Composable
private fun UpdateTitle(text: String) {
    Text(text, style = MaterialTheme.typography.titleLarge)
}

@Composable
private fun UpdateDetail(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant)
}

private fun versionLine(context: Context, manifest: UpdateManifest): String {
    val builds = if (manifest.versionName == BuildConfig.VERSION_NAME && manifest.versionCode != BuildConfig.VERSION_CODE)
        " · Build ${BuildConfig.VERSION_CODE} → ${manifest.versionCode}" else ""
    return "Promtuz ${manifest.versionName}$builds · ${Formatter.formatShortFileSize(context, manifest.size)}"
}
