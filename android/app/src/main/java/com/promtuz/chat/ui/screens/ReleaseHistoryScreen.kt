package com.promtuz.chat.ui.screens

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.SimpleScreen
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.ui.components.releaseHistory
import org.koin.androidx.compose.koinViewModel

@Composable
fun ReleaseHistoryScreen(viewModel: UpdateVM = koinViewModel()) {
    val notes by viewModel.history.collectAsState()
    val loading by viewModel.historyLoading.collectAsState()
    val error by viewModel.historyError.collectAsState()
    LaunchedEffect(viewModel) { viewModel.loadHistory() }
    SimpleScreen(title = { Text("Release history") }, connectionStatus = false) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(),
            verticalArrangement = Arrangement.spacedBy(24.dp)) {
            if (loading) item("loading") { LinearProgressIndicator() }
            error?.let { message -> item("error") {
                Text(message, color = MaterialTheme.colorScheme.onSurfaceVariant)
                TextButton(onClick = viewModel::loadHistory, enabled = !loading) { Text("Try again") }
            } }
            if (notes.isEmpty() && !loading && error == null) item("empty") {
                Text("No release notes available yet.", color = MaterialTheme.colorScheme.onSurfaceVariant)
            }
            releaseHistory(notes, BuildConfig.VERSION_CODE.toLong())
        }
    }
}
