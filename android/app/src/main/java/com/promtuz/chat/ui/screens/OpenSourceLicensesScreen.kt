package com.promtuz.chat.ui.screens

import android.content.ActivityNotFoundException
import android.widget.Toast
import androidx.activity.compose.LocalOnBackPressedDispatcherOwner
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLayoutDirection
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.ContactPickerHeader
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.GroupedActionRow
import com.promtuz.chat.ui.components.SimpleScreen
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

@Serializable
private data class LicenseEntry(val id: String, val coordinate: String, val name: String, val version: String, val license: String)

@Serializable
private data class LicenseNotice(val title: String, val file: String)

@Serializable
private data class LibraryLicense(
    val id: String,
    val name: String,
    val version: String,
    val license: String,
    val url: String,
    val notices: List<LicenseNotice>,
)

@Composable
fun OpenSourceLicensesScreen(onLibraryClick: (String) -> Unit) {
    val catalog by licenseAsset("licenses/index.json") { Json.decodeFromString<List<LicenseEntry>>(it) }
    val direction = LocalLayoutDirection.current
    val back = LocalOnBackPressedDispatcherOwner.current?.onBackPressedDispatcher
    var searching by rememberSaveable { mutableStateOf(false) }
    var query by rememberSaveable { mutableStateOf("") }
    val libraries = remember(catalog, query) {
        catalog?.getOrNull()?.filter {
            query.isBlank() || it.name.contains(query.trim(), ignoreCase = true) ||
                it.coordinate.contains(query.trim(), ignoreCase = true) ||
                it.license.contains(query.trim(), ignoreCase = true)
        }.orEmpty()
    }

    Scaffold(topBar = {
        Column(Modifier.background(MaterialTheme.colorScheme.background).statusBarsPadding()) {
            ContactPickerHeader(
                title = "Open source licenses",
                searching = searching,
                query = query,
                onQuery = { query = it },
                close = false,
                onBack = {
                    if (searching) { searching = false; query = "" }
                    else back?.onBackPressed()
                },
                onSearch = { searching = true },
                searchLabel = "Search libraries",
            )
        }
    }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().padding(
                start = padding.calculateLeftPadding(direction),
                end = padding.calculateRightPadding(direction),
            ),
            contentPadding = PaddingValues(18.dp, padding.calculateTopPadding() + 12.dp, 18.dp, 48.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            when {
                catalog == null -> item { LicenseLoading() }
                catalog?.isFailure == true -> item { LicenseError() }
                libraries.isEmpty() -> item {
                    Text("No libraries found", Modifier.padding(vertical = 24.dp),
                        color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                else -> itemsIndexed(libraries, key = { _, library -> library.id }) { index, library ->
                    GroupedActionRow(
                        library.name, index, libraries.size,
                        onClick = { onLibraryClick(library.id) },
                        supportingText = "${library.version} · ${library.license}",
                    ) {
                        Icon(painterResource(R.drawable.i_code), null, Modifier.size(26.dp))
                    }
                }
            }
        }
    }
}

@Composable
fun LibraryLicenseScreen(id: String) {
    val library by licenseAsset("licenses/$id.json") { Json.decodeFromString<LibraryLicense>(it) }
    val direction = LocalLayoutDirection.current
    val uriHandler = LocalUriHandler.current
    val context = LocalContext.current
    val colors = MaterialTheme.colorScheme
    val content = library?.getOrNull()
    val projectUrl = content?.url?.takeIf { it.startsWith("https://") || it.startsWith("http://") }

    SimpleScreen(
        title = { Text(content?.name ?: "License", maxLines = 1, overflow = TextOverflow.Ellipsis) },
        actions = {
            if (projectUrl != null) IconButton(onClick = {
                try { uriHandler.openUri(projectUrl) }
                catch (_: ActivityNotFoundException) {
                    Toast.makeText(context, "No browser available", Toast.LENGTH_SHORT).show()
                }
            }) {
                DrawableIcon(R.drawable.oi_external_link)
//                Icon(painterResource(R.drawable.oi_external_link), "Project website", Modifier.size(22.dp))
            }
        },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize().padding(
                start = padding.calculateLeftPadding(direction),
                end = padding.calculateRightPadding(direction),
            ),
            contentPadding = PaddingValues(18.dp, padding.calculateTopPadding() + 12.dp, 18.dp, 48.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            when {
                library == null -> item { LicenseLoading() }
                content == null -> item { LicenseError() }
                else -> {
                    item {
                        SelectionContainer {
                            Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                                Text("${content.version} · ${content.license}", style = MaterialTheme.typography.titleMedium)
                                Text(content.id.removePrefix("crate:"), color = colors.onSurfaceVariant,
                                    style = MaterialTheme.typography.bodyMedium)
                            }
                        }
                    }
                    items(content.notices, key = { it.file }) { notice ->
                        val text by licenseAsset(notice.file) { it }
                        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                            if (content.notices.size > 1) Text(notice.title,
                                color = colors.primary, style = MaterialTheme.typography.titleSmall)
                            when {
                                text == null -> LicenseLoading()
                                text?.isFailure == true -> LicenseError()
                                else -> SelectionContainer {
                                    Text(text!!.getOrThrow(), style = MaterialTheme.typography.bodyMedium)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun <T> licenseAsset(path: String, decode: (String) -> T): State<Result<T>?> {
    val assets = LocalContext.current.applicationContext.assets
    return produceState<Result<T>?>(null, assets, path) {
        value = withContext(Dispatchers.IO) {
            runCatching { assets.open(path).bufferedReader().use { decode(it.readText()) } }
        }
    }
}

@Composable
private fun LicenseLoading() {
    Box(Modifier.fillMaxSize().padding(32.dp), contentAlignment = Alignment.Center) {
        CircularProgressIndicator(Modifier.size(24.dp), strokeWidth = 2.dp)
    }
}

@Composable
private fun LicenseError() {
    Text("Couldn’t load licenses", Modifier.padding(vertical = 24.dp), color = MaterialTheme.colorScheme.error)
}
