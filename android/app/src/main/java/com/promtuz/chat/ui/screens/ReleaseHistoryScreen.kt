package com.promtuz.chat.ui.screens

import com.promtuz.chat.ui.components.TextButton
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.ui.Alignment
import com.promtuz.chat.ui.components.RowTitleStyle
import com.promtuz.chat.ui.theme.Promtuz
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import com.promtuz.chat.BuildConfig
import com.promtuz.chat.presentation.viewmodel.UpdateVM
import com.promtuz.chat.ui.components.GroupedScreen
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.ui.components.releaseHistory
import org.koin.androidx.compose.koinViewModel

@Composable
fun ReleaseHistoryScreen(viewModel: UpdateVM = koinViewModel()) {
    val notes by viewModel.history.collectAsState()
    val loading by viewModel.historyLoading.collectAsState()
    val error by viewModel.historyError.collectAsState()
    LaunchedEffect(viewModel) { viewModel.loadHistory() }
    GroupedScreen({ Text("Release history") }, connectionStatus = false) { padding ->
        LazyColumn(Modifier.fillMaxSize(), contentPadding = padding.listPadding(top = 6.dp, bottom = 24.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp)) {
            // Saved notes show at once; the network only adds newer ones, so it stays quiet unless nothing is shown.
            if (notes.isEmpty()) item("state") {
                Box(Modifier.fillParentMaxWidth().padding(top = 48.dp), contentAlignment = Alignment.Center) {
                    when {
                        loading -> CircularProgressIndicator(Modifier.size(28.dp), color = Promtuz.colors.muted, strokeWidth = 2.5.dp)
                        error != null -> Column(horizontalAlignment = Alignment.CenterHorizontally) {
                            Text(error!!, style = RowTitleStyle, color = Promtuz.colors.muted)
                            TextButton(onClick = viewModel::loadHistory) { Text("Try again") }
                        }
                        else -> Text("No release notes yet.", style = RowTitleStyle, color = Promtuz.colors.muted)
                    }
                }
            }
            releaseHistory(notes, BuildConfig.VERSION_CODE.toLong())
        }
    }
}
