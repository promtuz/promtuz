package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.theme.Radii
import android.content.Context
import android.text.format.Formatter
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.R
import com.promtuz.chat.navigation.Routes
import com.promtuz.chat.presentation.viewmodel.AppVM
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.*
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.ui.theme.Promtuz
import com.promtuz.chat.update.UpdateManifest
import com.promtuz.chat.update.UpdateState
import com.promtuz.chat.update.offered
import org.koin.compose.koinInject
import org.koin.androidx.compose.koinViewModel

@Composable
fun UpdateScreen(viewModel: UpdateVM = koinViewModel()) {
    val state by viewModel.state.collectAsState()
    val channel by viewModel.channel.collectAsState()
    val notes by viewModel.notes.collectAsState()
    val app = koinInject<AppVM>()
    val context = LocalContext.current
    var pendingChannel by remember { mutableStateOf<String?>(null) }

    LifecycleResumeEffect(viewModel) {
        viewModel.setScreenVisible(true)
        viewModel.check()
        onPauseOrDispose { viewModel.setScreenVisible(false) }
    }

    val c = Promtuz.colors
    GroupedScreen({ Text("Updates") }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(top = 6.dp, bottom = 24.dp),
        ) {
            item("status") {
                AnimatedContent(
                    targetState = state,
                    contentKey = { it::class },
                    transitionSpec = { fadeIn(Tweens.microInteraction(220)) togetherWith fadeOut(Tweens.microInteraction(160)) },
                    label = "updateStatus",
                ) { shown ->
                    val active = state::class == shown::class
                    val manifest = shown.offered
                    val switching = manifest?.versionCode == BuildConfig.VERSION_CODE && channel != viewModel.installedChannel
                    val offer = shown is UpdateState.Available || shown is UpdateState.Downloading || shown is UpdateState.Ready
                    Column {
                        Column(Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(c.card).padding(16.dp)) {
                            Row(horizontalArrangement = Arrangement.spacedBy(14.dp), verticalAlignment = Alignment.CenterVertically) {
                                val tint = when (shown) {
                                    UpdateState.None -> c.success
                                    is UpdateState.Error -> c.danger
                                    else -> if (offer) c.accent else c.muted
                                }
                                Box(Modifier.size(48.dp).clip(RoundedCornerShape(com.promtuz.chat.ui.theme.Radii.badge)).background(tint.copy(alpha = 0.16f)),
                                    contentAlignment = Alignment.Center) {
                                    when (shown) {
                                        UpdateState.None -> MorphIcon(MorphGlyph.Check, null, Modifier.size(24.dp), tint = tint)
                                        is UpdateState.Error -> DrawableIcon(R.drawable.oi_info, size = 24.dp, tint = tint)
                                        else -> DrawableIcon(R.drawable.i_download, size = 24.dp, tint = tint)
                                    }
                                }
                                Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
                                    Text(when (shown) {
                                        UpdateState.Unchecked -> "Check for updates"
                                        UpdateState.None -> "Promtuz is up to date"
                                        UpdateState.Checking -> "Checking for updates…"
                                        is UpdateState.Error -> "Couldn’t update Promtuz"
                                        else -> if (switching) "Promtuz ${channelLabel(channel)} is ready" else "Promtuz ${manifest?.versionName} is ready"
                                    }, style = TextStyle(fontSize = 17.sp, lineHeight = 22.sp, fontWeight = FontWeight.Medium), color = c.text)
                                    Text(when (shown) {
                                        is UpdateState.Error -> shown.message
                                        else -> manifest?.let { versionLine(context, it, channelLabel(channel)) }
                                            ?: "${BuildConfig.VERSION_NAME} · ${channelLabel(viewModel.installedChannel)}"
                                    }, style = RowSubtitleStyle, color = c.muted)
                                }
                            }
                            if (offer && notes.isNotEmpty()) {
                                Box(Modifier.padding(top = 14.dp).fillMaxWidth().height(1.dp).background(c.divider))
                                Text(if (switching) "About this release" else "Coming in this update", Modifier.padding(top = 12.dp, bottom = 10.dp),
                                    style = TextStyle(fontSize = 14.sp, lineHeight = 20.sp, fontWeight = FontWeight.Medium), color = c.accent)
                                UpdateNotes(notes)
                            }
                            if (offer) Spacer(Modifier.height(16.dp))
                            when (shown) {
                                is UpdateState.Available -> AppButton(if (switching) "Download ${channelLabel(channel)} build" else "Download and install",
                                    { viewModel.download(shown.manifest) }, Modifier.fillMaxWidth(), enabled = active)
                                is UpdateState.Ready -> {
                                    Text("Confirm the update in Android’s installer.", Modifier.padding(bottom = 10.dp), style = RowSubtitleStyle, color = c.muted)
                                    AppButton("Install", { viewModel.install(shown.manifest, shown.apk) }, Modifier.fillMaxWidth(), enabled = active)
                                }
                                is UpdateState.Downloading -> {
                                    LinearProgressIndicator({ shown.progress }, Modifier.fillMaxWidth(),
                                        color = c.accentFill, trackColor = c.switchOff, gapSize = 0.dp, drawStopIndicator = {})
                                    Row(Modifier.fillMaxWidth(), verticalAlignment = Alignment.CenterVertically) {
                                        Text("Downloading… ${Formatter.formatShortFileSize(context, (shown.manifest.size * shown.progress).toLong())} of " +
                                            Formatter.formatShortFileSize(context, shown.manifest.size),
                                            Modifier.weight(1f), style = RowSubtitleStyle, color = c.muted)
                                        TextButton(viewModel::cancelDownload, enabled = active) {
                                            Text("Cancel", style = TextStyle(fontSize = 14.sp, fontWeight = FontWeight.Medium), color = c.accent)
                                        }
                                    }
                                }
                                else -> {}
                            }
                        }
                        if (!offer) {
                            CardGap()
                            AppButton(if (shown is UpdateState.Error) "Try again" else "Check now", viewModel::check,
                                Modifier.fillMaxWidth(), kind = ButtonKind.Tonal, busy = shown == UpdateState.Checking, enabled = active)
                        }
                    }
                }
            }
            item("settings") {
                CardGap()
                SettingsCard {
                    SettingsChoiceRow(
                        "Update channel",
                        choices = listOf(
                            Choice("release", "Stable", "Tested releases"),
                            Choice("debug", "Debug", "Every build, for testing"),
                        ),
                        selected = channel,
                        onSelect = { option ->
                            if (option != channel) when (state) {
                                is UpdateState.Downloading, is UpdateState.Ready -> pendingChannel = option
                                else -> viewModel.switchChannel(option)
                            }
                        },
                    )
                    SettingsRow("Release history", trailing = Trailing.Chevron, onClick = { app.navigator.push(Routes.ReleaseHistory) })
                }
            }
        }
    }

    UpdateInstallPermissionDialog(state, viewModel)

    pendingChannel?.let { selected ->
        AppAlertDialog(
            onDismissRequest = { pendingChannel = null },
            title = { Text("Switch to ${channelLabel(selected)}?") },
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

internal fun channelLabel(channel: String) = if (channel == "release") "Stable" else channel.replaceFirstChar { it.uppercase() }

private fun versionLine(context: Context, manifest: UpdateManifest, channel: String): String {
    val builds = if (manifest.versionName == BuildConfig.VERSION_NAME && manifest.versionCode != BuildConfig.VERSION_CODE)
        " · Build ${BuildConfig.VERSION_CODE} → ${manifest.versionCode}" else ""
    return "${Formatter.formatShortFileSize(context, manifest.size)}$builds · $channel"
}
