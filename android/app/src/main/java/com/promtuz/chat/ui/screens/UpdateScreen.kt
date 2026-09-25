package com.promtuz.chat.ui.screens

import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.AlertDialog
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
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.MorphGlyph
import com.promtuz.chat.ui.components.AppDropMenu
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.MenuAction
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.releaseNotes
import com.promtuz.chat.update.offered
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.update.UpdateManifest
import com.promtuz.chat.update.UpdateState
import org.koin.androidx.compose.koinViewModel
import java.util.Locale

@Composable
fun UpdateScreen(viewModel: UpdateVM = koinViewModel()) {
    val state by viewModel.state.collectAsState()
    val channel by viewModel.channel.collectAsState()
    val notes by viewModel.notes.collectAsState()
    val context = LocalContext.current
    val direction = LocalLayoutDirection.current
    var pendingChannel by remember { mutableStateOf<String?>(null) }

    LifecycleResumeEffect(viewModel) {
        viewModel.setScreenVisible(true)
        val current = viewModel.state.value
        if (current is UpdateState.PermissionNeeded && context.packageManager.canRequestPackageInstalls()) {
            viewModel.install(current.manifest, current.apk)
        } else {
            viewModel.check()
        }
        onPauseOrDispose { viewModel.setScreenVisible(false) }
    }

    val channelActions = listOf("release", "debug").map { option ->
        MenuAction(
            "${option.replaceFirstChar { it.uppercase() }} channel",
            glyph = if (channel == option) MorphGlyph.Check else null,
        ) {
            if (option != channel) {
                when (state) {
                    is UpdateState.Downloading, is UpdateState.Ready, is UpdateState.PermissionNeeded -> pendingChannel = option
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
                groups = listOf(channelActions),
            )
        },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().padding(
                start = padding.calculateLeftPadding(direction),
                end = padding.calculateRightPadding(direction),
            ),
            contentPadding = PaddingValues(24.dp, padding.calculateTopPadding() + 40.dp, 24.dp, padding.calculateBottomPadding() + 32.dp),
            verticalArrangement = Arrangement.spacedBy(28.dp),
            horizontalAlignment = Alignment.CenterHorizontally,
        ) {
            item("app") {
                Image(painterResource(R.drawable.logo_colored), null, Modifier.size(88.dp))
            }
            item("status") {
                AnimatedContent(
                    targetState = state,
                    contentKey = { it::class },
                    contentAlignment = Alignment.TopCenter,
                    transitionSpec = {
                        (fadeIn(Tweens.microInteraction(220)) + slideInVertically(Tweens.microInteraction(220)) { it / 5 }) togetherWith
                            (fadeOut(Tweens.microInteraction(160)) + slideOutVertically(Tweens.microInteraction(160)) { -it / 5 })
                    },
                    label = "updateStatus",
                ) { shown ->
                    Column(
                        Modifier.fillMaxWidth(),
                        horizontalAlignment = Alignment.CenterHorizontally,
                        verticalArrangement = Arrangement.spacedBy(16.dp),
                    ) {
                        val active = state::class == shown::class
                        when (shown) {
                            UpdateState.Unchecked -> {
                                UpdateTitle("Check for updates")
                                UpdateAction("Check now", active, viewModel::check)
                            }
                            UpdateState.None -> {
                                UpdateTitle("You're up to date")
                                UpdateDetail("Promtuz ${BuildConfig.VERSION_NAME}")
                                UpdateAction("Check again", active, viewModel::check)
                            }
                            UpdateState.Checking -> {
                                UpdateTitle("Checking for updates")
                                LinearProgressIndicator(Modifier.fillMaxWidth().padding(vertical = 12.dp))
                            }
                            is UpdateState.Available -> {
                                UpdateTitle("Update available")
                                UpdateDetail(versionLine(shown.manifest))
                                UpdateAction("Download update", active) { viewModel.download(shown.manifest) }
                            }
                            is UpdateState.Downloading -> {
                                UpdateTitle("Downloading update")
                                UpdateDetail("Promtuz ${shown.manifest.versionName}")
                                LinearProgressIndicator({ shown.progress }, Modifier.fillMaxWidth())
                                UpdateDetail("${(shown.progress * 100).toInt()}% · ${formatSize(shown.manifest.size)}")
                                TextButton(viewModel::cancelDownload, enabled = active) { Text("Cancel download") }
                            }
                            is UpdateState.Ready -> {
                                UpdateTitle("Ready to install")
                                UpdateDetail(versionLine(shown.manifest))
                                UpdateAction("Install update", active) { viewModel.install(shown.manifest, shown.apk) }
                            }
                            is UpdateState.PermissionNeeded -> {
                                UpdateTitle("Allow installs")
                                UpdateDetail("Allow Promtuz to install updates in Android settings.")
                                UpdateAction("Open settings", active, viewModel::requestInstallPermission)
                            }
                            is UpdateState.Error -> {
                                UpdateTitle("Couldn't update Promtuz")
                                UpdateDetail(shown.message)
                                UpdateAction("Try again", active, viewModel::check)
                            }
                        }
                    }
                }
            }
            state.offered?.let { releaseNotes(notes, it.versionName) }
            item("channel") {
                Column(
                    Modifier.fillMaxWidth().padding(top = 8.dp),
                    horizontalAlignment = Alignment.CenterHorizontally,
                    verticalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    UpdateDetail("${channel.replaceFirstChar { it.uppercase() }} channel")
                    if (state != UpdateState.None) Text(
                        "Installed: ${BuildConfig.VERSION_NAME}",
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                        style = MaterialTheme.typography.bodySmall,
                    )
                }
            }
        }
    }

    pendingChannel?.let { selected ->
        AlertDialog(
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
private fun UpdateAction(label: String, enabled: Boolean, onClick: () -> Unit) {
    Button(onClick, Modifier.fillMaxWidth().heightIn(min = 52.dp), enabled = enabled) { Text(label) }
}

@Composable
private fun UpdateTitle(text: String) {
    Text(text, style = MaterialTheme.typography.headlineSmall, textAlign = TextAlign.Center)
}

@Composable
private fun UpdateDetail(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant, textAlign = TextAlign.Center)
}

private fun versionLine(manifest: UpdateManifest) = "Promtuz ${manifest.versionName} · ${formatSize(manifest.size)}"

internal fun formatSize(bytes: Long): String = when {
    bytes >= 1_000_000 -> String.format(Locale.ROOT, "%.1f MB", bytes / 1_000_000.0)
    bytes >= 1_000 -> String.format(Locale.ROOT, "%.0f KB", bytes / 1_000.0)
    else -> "$bytes B"
}
