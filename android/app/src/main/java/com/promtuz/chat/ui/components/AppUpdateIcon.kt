package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.IconButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.update.UpdateState
import org.koin.androidx.compose.koinViewModel

@Composable
fun AppUpdateIcon(onClick: () -> Unit, modifier: Modifier = Modifier, updates: UpdateVM = koinViewModel()) {
    val state by updates.state.collectAsState()
    val pending = state is UpdateState.Available || state is UpdateState.Downloading ||
        state is UpdateState.Ready || state is UpdateState.PermissionNeeded
    if (!pending) return

    IconButton(onClick, modifier) {
        val s = state
        // TODO: Temporarily allowing a separate progress indicator, will need xit included in the icon itself (lottie) later on
        if (s is UpdateState.Downloading) {
            CircularProgressIndicator({ s.progress }, Modifier.size(22.dp), strokeWidth = 2.dp)
        } else {
            Box { DrawableIcon(R.drawable.oi_update, desc = "Updates") }
        }
    }
}
