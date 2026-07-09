package com.promtuz.chat.ui.components

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.FilledIconButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.presentation.viewmodel.ChatVM

/** Bare composer — capped growing field + send. Draft lives in the VM so send clears it. */
@Composable
fun ChatBottomBar(viewModel: ChatVM) {
    val input by viewModel.input.collectAsState()
    Row(
        Modifier
            .fillMaxWidth()
            .navigationBarsPadding()
            .imePadding()
            .padding(8.dp),
        verticalAlignment = Alignment.Bottom,
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        OutlinedTextField(
            value = input,
            onValueChange = { viewModel.input.value = it },
            modifier = Modifier.weight(1f),
            placeholder = { Text("Message") },
            maxLines = 6,
        )
        FilledIconButton(onClick = viewModel::send, enabled = input.isNotBlank()) {
            DrawableIcon(R.drawable.i_send, Modifier.size(18.dp))
        }
    }
}
