package com.promtuz.chat.ui.screens

import androidx.compose.runtime.getValue
import com.promtuz.chat.ui.components.IconButton
import com.promtuz.chat.ui.components.cardFill
import android.content.ActivityNotFoundException
import android.widget.Toast
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.State
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import com.promtuz.chat.ui.components.listPadding
import com.promtuz.chat.R
import com.promtuz.chat.ui.components.ScreenScaffold
import com.promtuz.chat.ui.components.SearchTopBar
import com.promtuz.chat.ui.components.rememberScreenSearch
import com.promtuz.chat.ui.components.DrawableIcon
import com.promtuz.chat.ui.components.SettingsRow
import com.promtuz.chat.ui.components.Trailing
import com.promtuz.chat.ui.components.lazyCardRow
import com.promtuz.chat.ui.components.GroupedScreen
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.ui.draw.clip
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import com.promtuz.chat.ui.theme.Radii
import com.promtuz.chat.ui.components.RowSubtitleStyle
import com.promtuz.chat.ui.components.RowTitleStyle
import com.promtuz.chat.ui.theme.Promtuz
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
    val search = rememberScreenSearch()
    val query = search.query
    val libraries = remember(catalog, query) {
        catalog?.getOrNull()?.filter {
            query.isBlank() || it.name.contains(query.trim(), ignoreCase = true) ||
                it.coordinate.contains(query.trim(), ignoreCase = true) ||
                it.license.contains(query.trim(), ignoreCase = true)
        }.orEmpty()
    }

    ScreenScaffold(containerColor = Promtuz.colors.groupedPage, topBar = { scrollBehavior ->
        SearchTopBar("Open source licenses", search, "Search libraries", scrollBehavior)
    }) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(top = 6.dp, bottom = 48.dp),
            verticalArrangement = Arrangement.Top,
        ) {
            when {
                catalog == null -> item { LicenseLoading() }
                catalog?.isFailure == true -> item { LicenseError() }
                libraries.isEmpty() -> item {
                    Text("No libraries found", Modifier.padding(16.dp), style = RowSubtitleStyle, color = Promtuz.colors.muted)
                }
                else -> itemsIndexed(libraries, key = { _, library -> library.id }) { index, library ->
                    SettingsRow(
                        library.name,
                        Modifier.lazyCardRow(index, libraries.size),
                        subtitle = "${library.version} · ${library.license}",
                        trailing = Trailing.Chevron,
                        onClick = { onLibraryClick(library.id) },
                    )
                }
            }
        }
    }
}

@Composable
fun LibraryLicenseScreen(id: String) {
    val library by licenseAsset("licenses/$id.json") { Json.decodeFromString<LibraryLicense>(it) }
    val uriHandler = LocalUriHandler.current
    val context = LocalContext.current
    val c = Promtuz.colors
    val content = library?.getOrNull()
    val projectUrl = content?.url?.takeIf { it.startsWith("https://") || it.startsWith("http://") }

    GroupedScreen(
        title = { Text(content?.name ?: "License", maxLines = 1, overflow = TextOverflow.Ellipsis) },
        actions = {
            if (projectUrl != null) IconButton(onClick = {
                try { uriHandler.openUri(projectUrl) }
                catch (_: ActivityNotFoundException) {
                    Toast.makeText(context, "No browser available", Toast.LENGTH_SHORT).show()
                }
            }) {
                DrawableIcon(R.drawable.oi_external_link, desc = "Project website")
            }
        },
    ) { padding ->
        LazyColumn(
            Modifier.fillMaxSize(),
            contentPadding = padding.listPadding(top = 6.dp, bottom = 48.dp),
            verticalArrangement = Arrangement.spacedBy(20.dp),
        ) {
            when {
                library == null -> item { LicenseLoading() }
                content == null -> item { LicenseError() }
                else -> {
                    item {
                        SelectionContainer {
                            Column(
                                Modifier.fillMaxWidth().clip(RoundedCornerShape(Radii.card)).background(cardFill())
                                    .padding(horizontal = 16.dp, vertical = 14.dp),
                                verticalArrangement = Arrangement.spacedBy(4.dp),
                            ) {
                                Text("${content.version} · ${content.license}", style = TextStyle(fontSize = 18.sp, lineHeight = 24.sp, fontWeight = FontWeight.Medium), color = c.text)
                                Text(source(content.id), style = RowSubtitleStyle.copy(fontFamily = FontFamily.Monospace), color = c.muted)
                            }
                        }
                    }
                    items(content.notices, key = { it.file }) { notice ->
                        val text by licenseAsset(notice.file, ::reflow)
                        Column(Modifier.padding(horizontal = 16.dp), verticalArrangement = Arrangement.spacedBy(12.dp)) {
                            if (content.notices.size > 1) Text(notice.title, style = RowTitleStyle.copy(fontWeight = FontWeight.Medium), color = c.accent)
                            when {
                                text == null -> LicenseLoading()
                                text?.isFailure == true -> LicenseError()
                                else -> SelectionContainer {
                                    Text(text!!.getOrThrow(), style = RowSubtitleStyle.copy(lineHeight = 21.sp), color = c.muted)
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

// Where the package lives, as people would type it: crates.io/name or the Maven group:artifact.
private fun source(id: String): String = when {
    id.startsWith("crate:") -> "crates.io/" + id.removePrefix("crate:").substringBefore(':')
    else -> id.substringBeforeLast(':')
}

private val ListItem = Regex("""^(\d+\.|\([a-z0-9]+\)|[-*•])\s""")

// Joins hard-wrapped lines so paragraphs wrap to the screen; blank lines and list items still break.
private fun reflow(text: String): String = buildString {
    for (raw in text.lines()) {
        val line = raw.trim()
        when {
            line.isEmpty() -> if (isNotEmpty() && !endsWith("\n\n")) append(if (endsWith("\n")) "\n" else "\n\n")
            isEmpty() || endsWith("\n") -> append(line)
            ListItem.containsMatchIn(line) -> append('\n').append(line)
            else -> append(' ').append(line)
        }
    }
}.trim()

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
        CircularProgressIndicator(Modifier.size(24.dp), color = Promtuz.colors.accent, strokeWidth = 2.dp)
    }
}

@Composable
private fun LicenseError() {
    Text("Couldn’t load licenses", Modifier.padding(16.dp), style = RowSubtitleStyle, color = Promtuz.colors.danger)
}
