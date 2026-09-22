package com.promtuz.chat.ui.screens

import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBars
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.Button
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LifecycleResumeEffect
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.releaseNotes
import com.promtuz.chat.ui.constants.Tweens
import com.promtuz.chat.update.UpdateManifest
import com.promtuz.chat.update.UpdateState
import org.koin.androidx.compose.koinViewModel

/** Covers the whole app while the installed build can no longer be used. */
@Composable
fun RequiredUpdateHost(viewModel: UpdateVM = koinViewModel()) {
    val required by viewModel.required.collectAsState()
    var shown by remember { mutableStateOf(required) }
    if (required != null) shown = required
    AnimatedVisibility(
        required != null,
        enter = fadeIn(Tweens.microInteraction(240)) + slideInVertically(Tweens.microInteraction(240)) { it / 12 },
        exit = fadeOut(Tweens.microInteraction(200)),
    ) {
        shown?.let { RequiredUpdateScreen(it, viewModel) }
    }
}

@Composable
private fun RequiredUpdateScreen(manifest: UpdateManifest, viewModel: UpdateVM) {
    val state by viewModel.state.collectAsState()
    val notes by viewModel.notes.collectAsState()
    val context = LocalContext.current
    val colors = MaterialTheme.colorScheme

    BackHandler { }
    LifecycleResumeEffect(viewModel) {
        viewModel.setScreenVisible(true)
        val current = viewModel.state.value
        if (current is UpdateState.PermissionNeeded && context.packageManager.canRequestPackageInstalls()) {
            viewModel.install(current.manifest, current.apk)
        }
        onPauseOrDispose { viewModel.setScreenVisible(false) }
    }

    // A Surface, not a Box: it sets the content colour and keeps touches off the app below.
    Surface(Modifier.fillMaxSize(), color = colors.background) { Column {
        LazyColumn(
            Modifier.weight(1f).fillMaxWidth(),
            contentPadding = PaddingValues(
                24.dp, WindowInsets.statusBars.asPaddingValues().calculateTopPadding() + 48.dp, 24.dp, 24.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(32.dp),
        ) {
            item("title") {
                Column {
                    Image(painterResource(R.drawable.logo_colored), null, Modifier.size(64.dp))
                    Spacer(Modifier.height(24.dp))
                    Text("Update required", style = MaterialTheme.typography.headlineLarge)
                    Spacer(Modifier.height(6.dp))
                    Text(
                        "Promtuz ${manifest.versionName} · ${formatSize(manifest.size)}",
                        style = MaterialTheme.typography.bodyMedium,
                        color = colors.onSurfaceVariant,
                    )
                    Spacer(Modifier.height(20.dp))
                    Text(
                        "Your version of Promtuz no longer works with the network. Update to keep chatting.",
                        style = MaterialTheme.typography.bodyLarge,
                    )
                }
            }
            releaseNotes(notes, manifest.versionName)
        }

        AnimatedContent(
            targetState = state,
            contentKey = { it::class },
            transitionSpec = {
                (fadeIn(Tweens.microInteraction(220)) + slideInVertically(Tweens.microInteraction(220)) { it / 5 }) togetherWith
                    (fadeOut(Tweens.microInteraction(160)) + slideOutVertically(Tweens.microInteraction(160)) { -it / 5 })
            },
            label = "requiredUpdateAction",
        ) { shown ->
            Column(
                Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 24.dp)
                    .navigationBarsPadding()
                    .padding(top = 12.dp, bottom = 16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                val active = state::class == shown::class
                when (shown) {
                    UpdateState.Checking -> LinearProgressIndicator(Modifier.fillMaxWidth().padding(vertical = 12.dp))
                    is UpdateState.Downloading -> {
                        LinearProgressIndicator({ shown.progress }, Modifier.fillMaxWidth())
                        Detail("Downloading · ${(shown.progress * 100).toInt()}% of ${formatSize(shown.manifest.size)}")
                    }
                    is UpdateState.Ready -> Action("Install", active) { viewModel.install(shown.manifest, shown.apk) }
                    is UpdateState.PermissionNeeded -> {
                        Detail("Allow Promtuz to install updates in Android settings.")
                        Action("Open settings", active, viewModel::requestInstallPermission)
                    }
                    is UpdateState.Error -> {
                        Detail(shown.message)
                        Action("Try again", active, viewModel::check)
                    }
                    else -> Action("Download", active) { viewModel.download(manifest) }
                }
            }
        }
    } }
}

@Composable
private fun Action(label: String, enabled: Boolean, onClick: () -> Unit) {
    Button(onClick, Modifier.fillMaxWidth().heightIn(min = 52.dp), enabled = enabled) { Text(label) }
}

@Composable
private fun Detail(text: String) {
    Text(text, style = MaterialTheme.typography.bodyMedium, color = MaterialTheme.colorScheme.onSurfaceVariant)
}
