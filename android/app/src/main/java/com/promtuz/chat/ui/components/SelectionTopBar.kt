package com.promtuz.chat.ui.components

import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import com.promtuz.chat.R
import com.promtuz.chat.ui.appearance.chatBarHaze
import com.promtuz.chat.ui.util.freezeOnExit
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeEffect

/** Contextual bar while messages are selected: count + bulk actions. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SelectionTopBar(
    count: Int,
    haze: HazeState,
    onClose: () -> Unit,
    onCopy: () -> Unit,
    onDelete: () -> Unit,
) {
    TopAppBar(
        title = { Text("$count", style = MaterialTheme.typography.titleMediumEmphasized) },
        navigationIcon = {
            IconButton(onClick = onClose) { DrawableIcon(R.drawable.i_close) }
        },
        actions = {
            IconButton(onClick = onCopy) { DrawableIcon(R.drawable.i_copy) }
            IconButton(onClick = onDelete) {
                DrawableIcon(R.drawable.i_delete, tint = MaterialTheme.colorScheme.error)
            }
        },
        modifier = Modifier.freezeOnExit().hazeEffect(haze, chatBarHaze()),
        colors = TopAppBarDefaults.topAppBarColors(containerColor = Color.Transparent),
    )
}
