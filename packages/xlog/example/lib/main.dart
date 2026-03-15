import 'dart:io';

import 'package:flutter/material.dart';
import 'package:intl/intl.dart';
import 'package:path_provider/path_provider.dart';
import 'package:xlog/xlog.dart';

const _displayFontFamily = 'Avenir Next';
const _displayFontFallback = <String>['Helvetica Neue', 'Segoe UI', 'Arial'];
const _monoFontFamily = 'Menlo';
const _monoFontFallback = <String>['SF Mono', 'Consolas', 'Courier New'];

TextTheme _displayTextTheme(TextTheme base) {
  return base.apply(
    fontFamily: _displayFontFamily,
    displayColor: const Color(0xFF0F172A),
    bodyColor: const Color(0xFF0F172A),
  );
}

TextStyle _monoStyle({
  double? fontSize,
  FontWeight? fontWeight,
  Color? color,
  double? height,
}) {
  return TextStyle(
    fontFamily: _monoFontFamily,
    fontFamilyFallback: _monoFontFallback,
    fontSize: fontSize,
    fontWeight: fontWeight,
    color: color,
    height: height,
  );
}

void main() {
  runApp(const MarsXlogDiagnosticsApp());
}

class MarsXlogDiagnosticsApp extends StatelessWidget {
  const MarsXlogDiagnosticsApp({super.key});

  @override
  Widget build(BuildContext context) {
    final baseTheme = ThemeData(
      colorScheme: ColorScheme.fromSeed(
        seedColor: const Color(0xFF146C94),
        brightness: Brightness.light,
      ),
      useMaterial3: true,
    );

    return MaterialApp(
      debugShowCheckedModeBanner: false,
      title: 'xlog example',
      theme: baseTheme.copyWith(
        scaffoldBackgroundColor: const Color(0xFFF3EEE4),
        textTheme: _displayTextTheme(baseTheme.textTheme),
        colorScheme: baseTheme.colorScheme.copyWith(
          primary: const Color(0xFF14532D),
          secondary: const Color(0xFFE07A2F),
          surface: const Color(0xFFFFFBF5),
        ),
      ),
      home: const DiagnosticsHomePage(),
    );
  }
}

class DiagnosticsHomePage extends StatefulWidget {
  const DiagnosticsHomePage({super.key});

  @override
  State<DiagnosticsHomePage> createState() => _DiagnosticsHomePageState();
}

class _DiagnosticsHomePageState extends State<DiagnosticsHomePage> {
  final _prefixController = TextEditingController(
    text: 'flutter_native_assets',
  );
  final _tagController = TextEditingController(text: 'flutter');
  final _messageController = TextEditingController(
    text: 'Mars Xlog from Flutter through Dart native assets and Rust FFI.',
  );
  final _iterationsController = TextEditingController(text: '20000');
  final _threadsController = TextEditingController(text: '4');
  final _messageSizeController = TextEditingController(text: '160');
  final _timeFormat = DateFormat('MM-dd HH:mm:ss');

  MarsXlogLogger? _logger;
  MarsXlogAppenderMode _appenderMode = MarsXlogAppenderMode.async;
  MarsXlogCompressMode _compressMode = MarsXlogCompressMode.zstd;
  MarsXlogLevel _benchmarkLevel = MarsXlogLevel.info;
  bool _consoleOpen = true;
  bool _busyInitializing = false;
  bool _busyListing = false;
  bool _busyMetrics = false;
  bool _busyDecode = false;
  bool _busyBenchmark = false;

  String? _logDir;
  String? _cacheDir;
  List<MarsXlogLogFile> _files = const [];
  String _decodedFileContent = '';
  String _metricsSnapshot = '';
  String? _selectedFile;
  MarsXlogBenchmarkReport? _benchmarkReport;
  List<_MetricTileData> _metricTiles = const [];
  final List<String> _activity = <String>[];

  @override
  void initState() {
    super.initState();
    _bootstrap();
  }

  @override
  void dispose() {
    _logger?.dispose();
    _prefixController.dispose();
    _tagController.dispose();
    _messageController.dispose();
    _iterationsController.dispose();
    _threadsController.dispose();
    _messageSizeController.dispose();
    super.dispose();
  }

  Future<void> _bootstrap() async {
    final supportDir = await getApplicationSupportDirectory();
    final baseDir = Directory('${supportDir.path}/xlog');
    final logDir = Directory('${baseDir.path}/logs');
    final cacheDir = Directory('${baseDir.path}/cache');
    await logDir.create(recursive: true);
    await cacheDir.create(recursive: true);

    if (!mounted) {
      return;
    }

    setState(() {
      _logDir = logDir.path;
      _cacheDir = cacheDir.path;
    });
    _pushActivity('Sandbox ready at ${baseDir.path}');
  }

  void _pushActivity(String message) {
    final stamped = '${_timeFormat.format(DateTime.now())}  $message';
    setState(() {
      _activity.insert(0, stamped);
      if (_activity.length > 14) {
        _activity.removeLast();
      }
    });
  }

  void _showError(Object error) {
    final message = error.toString();
    _pushActivity(message);
    ScaffoldMessenger.of(context).showSnackBar(
      SnackBar(
        content: Text(message),
        backgroundColor: const Color(0xFF8B1E3F),
      ),
    );
  }

  Future<void> _initializeLogger() async {
    if (_logDir == null || _cacheDir == null) {
      return;
    }

    setState(() => _busyInitializing = true);
    try {
      _logger?.dispose();
      final logger = MarsXlogLogger.open(
        MarsXlogConfig(
          logDir: _logDir!,
          cacheDir: _cacheDir!,
          namePrefix: _prefixController.text.trim(),
          appenderMode: _appenderMode,
          compressMode: _compressMode,
          enableConsole: _consoleOpen,
          maxFileSizeBytes: 4 * 1024 * 1024,
          maxAliveTimeSeconds: 3 * 24 * 60 * 60,
        ),
        level: MarsXlogLevel.debug,
      );

      logger.info(
        'Logger initialized for Flutter diagnostics.',
        tag: _tagController.text.trim(),
      );
      logger.logWithMeta(
        MarsXlogLevel.warn,
        'Metadata path active for Dart -> Rust -> xlog.',
        tag: 'meta-case',
        file: 'example/lib/main.dart',
        functionName: '_initializeLogger',
        line: 144,
      );
      logger.flush();

      if (!mounted) {
        logger.dispose();
        return;
      }

      setState(() {
        _logger = logger;
        _decodedFileContent = '';
        _selectedFile = null;
      });
      _pushActivity(
        'Logger `${logger.namePrefix()}` opened in ${_appenderMode.label}/${_compressMode.label}',
      );
      await _refreshFiles();
      await _refreshMetrics();
    } catch (error) {
      _showError(error);
    } finally {
      if (mounted) {
        setState(() => _busyInitializing = false);
      }
    }
  }

  Future<void> _writeSingleCase() async {
    final logger = _logger;
    if (logger == null) {
      return;
    }

    try {
      logger.info(
        _messageController.text.trim(),
        tag: _tagController.text.trim(),
      );
      logger.flush(sync: false);
      _pushActivity('Single case written through dart:ffi');
      await _refreshFiles();
      await _refreshMetrics();
    } catch (error) {
      _showError(error);
    }
  }

  Future<void> _writeBurstCase() async {
    final logger = _logger;
    if (logger == null) {
      return;
    }

    final stopwatch = Stopwatch()..start();
    try {
      for (var index = 0; index < 240; index++) {
        logger.debug(
          'burst-line=$index payload=${_messageController.text.trim()}',
          tag: _tagController.text.trim(),
        );
      }
      logger.flush();
      stopwatch.stop();
      _pushActivity(
        'Burst case wrote 240 lines in ${stopwatch.elapsedMilliseconds} ms',
      );
      await _refreshFiles();
      await _refreshMetrics();
    } catch (error) {
      _showError(error);
    }
  }

  Future<void> _writeMetaCase() async {
    final logger = _logger;
    if (logger == null) {
      return;
    }

    try {
      logger.logWithMeta(
        MarsXlogLevel.error,
        'Synthetic crash breadcrumb for file viewer validation.',
        tag: 'case/error',
        file: 'example/lib/main.dart',
        functionName: '_writeMetaCase',
        line: 215,
      );
      logger.logWithMeta(
        MarsXlogLevel.info,
        'Business event: cart checkout submitted.',
        tag: 'case/biz',
        file: 'lib/cart_service.dart',
        functionName: 'submitOrder',
        line: 88,
      );
      logger.flush();
      _pushActivity('Structured and metadata-heavy scenarios appended');
      await _refreshFiles();
      await _refreshMetrics();
    } catch (error) {
      _showError(error);
    }
  }

  Future<void> _refreshFiles() async {
    final logger = _logger;
    if (logger == null) {
      return;
    }

    setState(() => _busyListing = true);
    try {
      final files = logger.listLogFiles(limit: 40);
      if (!mounted) {
        return;
      }
      setState(() => _files = files);
      _pushActivity('Indexed ${files.length} log artifacts');
    } catch (error) {
      _showError(error);
    } finally {
      if (mounted) {
        setState(() => _busyListing = false);
      }
    }
  }

  Future<void> _openLogFile(MarsXlogLogFile file) async {
    setState(() => _busyDecode = true);
    try {
      final decoded = await MarsXlog.decodeLogFileAsync(file.path);
      if (!mounted) {
        return;
      }
      setState(() {
        _decodedFileContent = decoded;
        _selectedFile = file.path;
      });
      _pushActivity('Decoded ${file.fileName}');
    } catch (error) {
      _showError(error);
    } finally {
      if (mounted) {
        setState(() => _busyDecode = false);
      }
    }
  }

  Future<void> _refreshMetrics() async {
    setState(() => _busyMetrics = true);
    try {
      final metrics = await MarsXlog.readMetricsSnapshotAsync();
      if (!mounted) {
        return;
      }
      setState(() {
        _metricsSnapshot = metrics;
        _metricTiles = _extractMetrics(metrics);
      });
      _pushActivity('Prometheus metrics snapshot refreshed');
    } catch (error) {
      _showError(error);
    } finally {
      if (mounted) {
        setState(() => _busyMetrics = false);
      }
    }
  }

  Future<void> _runBenchmark() async {
    final logger = _logger;
    if (logger == null) {
      return;
    }

    final iterations = int.tryParse(_iterationsController.text) ?? 0;
    final threads = int.tryParse(_threadsController.text) ?? 1;
    final messageBytes = int.tryParse(_messageSizeController.text) ?? 160;

    setState(() => _busyBenchmark = true);
    try {
      final report = await logger.runBenchmarkAsync(
        iterations: iterations,
        threads: threads,
        messageBytes: messageBytes,
        level: _benchmarkLevel,
        tag: _tagController.text.trim(),
      );
      if (!mounted) {
        return;
      }
      setState(() => _benchmarkReport = report);
      _pushActivity(
        'Native stress finished: ${_formatCompact(report.linesPerSecond)} lines/s',
      );
      await _refreshFiles();
      await _refreshMetrics();
    } catch (error) {
      _showError(error);
    } finally {
      if (mounted) {
        setState(() => _busyBenchmark = false);
      }
    }
  }

  @override
  Widget build(BuildContext context) {
    final loggerReady = _logger != null;

    return Scaffold(
      body: SafeArea(
        child: LayoutBuilder(
          builder: (context, constraints) {
            final wide = constraints.maxWidth >= 1180;
            final mainPanels = <Widget>[
              _buildHero(loggerReady),
              _buildControlsCard(loggerReady),
              _buildScenarioCard(loggerReady),
              _buildBenchmarkCard(loggerReady),
            ];
            final sidePanels = <Widget>[
              _buildFilesCard(),
              _buildMetricsCard(),
              _buildActivityCard(),
            ];

            return SingleChildScrollView(
              padding: const EdgeInsets.all(20),
              child: wide
                  ? Row(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Expanded(flex: 6, child: Column(children: mainPanels)),
                        const SizedBox(width: 20),
                        Expanded(flex: 5, child: Column(children: sidePanels)),
                      ],
                    )
                  : Column(children: [...mainPanels, ...sidePanels]),
            );
          },
        ),
      ),
    );
  }

  Widget _buildHero(bool loggerReady) {
    return Container(
      width: double.infinity,
      margin: const EdgeInsets.only(bottom: 20),
      padding: const EdgeInsets.all(24),
      decoration: BoxDecoration(
        borderRadius: BorderRadius.circular(28),
        gradient: const LinearGradient(
          colors: [Color(0xFF10212B), Color(0xFF1B4332), Color(0xFF2A9D8F)],
          begin: Alignment.topLeft,
          end: Alignment.bottomRight,
        ),
        boxShadow: const [
          BoxShadow(
            color: Color(0x26000000),
            blurRadius: 32,
            offset: Offset(0, 18),
          ),
        ],
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Wrap(
            spacing: 10,
            runSpacing: 10,
            children: [
              _heroChip('Native assets', Colors.white.withValues(alpha: 0.16)),
              _heroChip(
                'Rust xlog',
                const Color(0xFFE07A2F).withValues(alpha: 0.2),
              ),
              _heroChip(
                'Prometheus metrics',
                const Color(0xFFA7F3D0).withValues(alpha: 0.16),
              ),
            ],
          ),
          const SizedBox(height: 18),
          Text(
            'xlog diagnostics',
            style: Theme.of(context).textTheme.headlineMedium?.copyWith(
              color: Colors.white,
              fontWeight: FontWeight.w700,
            ),
          ),
          const SizedBox(height: 10),
          Text(
            '用 Flutter 最新 native assets 直接加载 Rust 动态库，'
            '在同一个 example 里完成日志写入、压力测试、文件查看和 metrics 观测。',
            style: Theme.of(context).textTheme.bodyLarge?.copyWith(
              color: Colors.white.withValues(alpha: 0.88),
              height: 1.45,
            ),
          ),
          const SizedBox(height: 22),
          Wrap(
            spacing: 14,
            runSpacing: 14,
            children: [
              _statusTile(
                title: 'Logger',
                value: loggerReady ? 'LIVE' : 'IDLE',
                accent: loggerReady
                    ? const Color(0xFFA7F3D0)
                    : const Color(0xFFFDE68A),
              ),
              _statusTile(
                title: 'Log Dir',
                value: _logDir ?? 'bootstrapping',
                accent: Colors.white,
              ),
              _statusTile(
                title: 'Files',
                value: '${_files.length}',
                accent: const Color(0xFFE0FBFC),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildControlsCard(bool loggerReady) {
    return _panel(
      title: 'Engine Control',
      subtitle: '初始化 logger，并固定 name prefix、压缩方式和 appender 模式。',
      child: Column(
        children: [
          Row(
            children: [
              Expanded(
                child: _textField(
                  controller: _prefixController,
                  label: 'Name Prefix',
                  hint: 'flutter_native_assets',
                ),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: _textField(
                  controller: _tagController,
                  label: 'Default Tag',
                  hint: 'flutter',
                ),
              ),
            ],
          ),
          const SizedBox(height: 14),
          Row(
            children: [
              Expanded(
                child: _segmentedField<MarsXlogAppenderMode>(
                  label: 'Appender',
                  values: MarsXlogAppenderMode.values,
                  current: _appenderMode,
                  labelOf: (value) => value.label,
                  onChanged: (value) => setState(() => _appenderMode = value),
                ),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: _segmentedField<MarsXlogCompressMode>(
                  label: 'Compression',
                  values: MarsXlogCompressMode.values,
                  current: _compressMode,
                  labelOf: (value) => value.label,
                  onChanged: (value) => setState(() => _compressMode = value),
                ),
              ),
            ],
          ),
          const SizedBox(height: 14),
          SwitchListTile.adaptive(
            contentPadding: EdgeInsets.zero,
            title: const Text('Mirror logs to console'),
            subtitle: Text(
              _consoleOpen
                  ? 'Console output enabled'
                  : 'Console output disabled',
            ),
            value: _consoleOpen,
            onChanged: (value) => setState(() => _consoleOpen = value),
          ),
          const SizedBox(height: 8),
          Row(
            children: [
              Expanded(
                child: SelectableText(
                  'logDir: ${_logDir ?? 'bootstrapping...'}\ncacheDir: ${_cacheDir ?? 'bootstrapping...'}',
                  style: _monoStyle(
                    fontSize: 12,
                    color: const Color(0xFF334155),
                    height: 1.5,
                  ),
                ),
              ),
              const SizedBox(width: 12),
              FilledButton.icon(
                onPressed: _busyInitializing ? null : _initializeLogger,
                icon: _busyInitializing
                    ? const SizedBox.square(
                        dimension: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Icon(Icons.bolt),
                label: const Text('Initialize'),
              ),
            ],
          ),
          if (loggerReady) ...[
            const SizedBox(height: 14),
            Align(
              alignment: Alignment.centerLeft,
              child: Text(
                '当前 logger: ${_logger?.namePrefix() ?? '-'}',
                style: Theme.of(context).textTheme.titleMedium,
              ),
            ),
          ],
        ],
      ),
    );
  }

  Widget _buildScenarioCard(bool loggerReady) {
    return _panel(
      title: 'Real Cases',
      subtitle: '真实业务案例、批量写入和 metadata 场景，便于直接验证 Flutter 集成。',
      child: Column(
        children: [
          _textField(
            controller: _messageController,
            label: 'Message Payload',
            hint: 'business log line',
            maxLines: 2,
          ),
          const SizedBox(height: 14),
          Wrap(
            spacing: 10,
            runSpacing: 10,
            children: [
              FilledButton.tonalIcon(
                onPressed: loggerReady ? _writeSingleCase : null,
                icon: const Icon(Icons.edit_note),
                label: const Text('Single Log'),
              ),
              FilledButton.tonalIcon(
                onPressed: loggerReady ? _writeMetaCase : null,
                icon: const Icon(Icons.route),
                label: const Text('Meta Case'),
              ),
              FilledButton.icon(
                onPressed: loggerReady ? _writeBurstCase : null,
                icon: const Icon(Icons.auto_awesome_motion),
                label: const Text('Burst 240'),
              ),
              OutlinedButton.icon(
                onPressed: loggerReady ? () => _logger?.flush() : null,
                icon: const Icon(Icons.save_alt),
                label: const Text('Flush'),
              ),
              OutlinedButton.icon(
                onPressed: loggerReady && !_busyListing ? _refreshFiles : null,
                icon: const Icon(Icons.folder_open),
                label: const Text('Refresh Files'),
              ),
            ],
          ),
        ],
      ),
    );
  }

  Widget _buildBenchmarkCard(bool loggerReady) {
    final report = _benchmarkReport;
    return _panel(
      title: 'Stress Lab',
      subtitle: 'benchmark 在 Rust 侧多线程执行，避免把 FFI 边界开销混进 xlog 核心吞吐。',
      child: Column(
        children: [
          Row(
            children: [
              Expanded(
                child: _textField(
                  controller: _iterationsController,
                  label: 'Iterations',
                  hint: '20000',
                ),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: _textField(
                  controller: _threadsController,
                  label: 'Threads',
                  hint: '4',
                ),
              ),
              const SizedBox(width: 12),
              Expanded(
                child: _textField(
                  controller: _messageSizeController,
                  label: 'Message Bytes',
                  hint: '160',
                ),
              ),
            ],
          ),
          const SizedBox(height: 14),
          _segmentedField<MarsXlogLevel>(
            label: 'Benchmark Level',
            values: const [
              MarsXlogLevel.debug,
              MarsXlogLevel.info,
              MarsXlogLevel.warn,
              MarsXlogLevel.error,
            ],
            current: _benchmarkLevel,
            labelOf: (value) => value.label,
            onChanged: (value) => setState(() => _benchmarkLevel = value),
          ),
          const SizedBox(height: 14),
          Row(
            children: [
              FilledButton.icon(
                onPressed: loggerReady && !_busyBenchmark
                    ? _runBenchmark
                    : null,
                icon: _busyBenchmark
                    ? const SizedBox.square(
                        dimension: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Icon(Icons.speed),
                label: const Text('Run Native Stress'),
              ),
              const SizedBox(width: 12),
              if (report != null)
                Expanded(
                  child: Wrap(
                    spacing: 10,
                    runSpacing: 10,
                    children: [
                      _metricBadge(
                        'Elapsed',
                        '${report.elapsed.inMilliseconds} ms',
                      ),
                      _metricBadge(
                        'Lines/s',
                        _formatCompact(report.linesPerSecond),
                      ),
                      _metricBadge(
                        'Bytes/s',
                        _formatBytesPerSecond(report.bytesPerSecond),
                      ),
                    ],
                  ),
                ),
            ],
          ),
          if (report != null) ...[
            const SizedBox(height: 14),
            Align(
              alignment: Alignment.centerLeft,
              child: Text(
                'Last artifact: ${report.currentLogPath ?? 'n/a'}',
                style: _monoStyle(fontSize: 12),
              ),
            ),
          ],
        ],
      ),
    );
  }

  Widget _buildFilesCard() {
    return _panel(
      title: 'Log Files',
      subtitle: '浏览当前日志目录，点击即可用 Rust 侧 decoder 解析 xlog 内容。',
      child: Column(
        children: [
          Row(
            children: [
              FilledButton.tonalIcon(
                onPressed: _logger != null && !_busyListing
                    ? _refreshFiles
                    : null,
                icon: _busyListing
                    ? const SizedBox.square(
                        dimension: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Icon(Icons.refresh),
                label: const Text('Refresh'),
              ),
              const SizedBox(width: 12),
              Text(
                '${_files.length} files',
                style: Theme.of(context).textTheme.titleMedium,
              ),
            ],
          ),
          const SizedBox(height: 16),
          if (_files.isEmpty)
            _emptyState(
              'No log files yet. Initialize the logger and write some cases.',
            )
          else
            Column(
              children: [
                for (final file in _files.take(8))
                  Container(
                    margin: const EdgeInsets.only(bottom: 10),
                    decoration: BoxDecoration(
                      color: const Color(0xFFF8F5EF),
                      borderRadius: BorderRadius.circular(18),
                      border: Border.all(
                        color: _selectedFile == file.path
                            ? const Color(0xFF14532D)
                            : const Color(0xFFD9D3C7),
                      ),
                    ),
                    child: ListTile(
                      title: Text(file.fileName),
                      subtitle: Text(
                        '${file.extension.toUpperCase()}  ·  ${_formatBytes(file.sizeBytes.toDouble())}  ·  ${_timeFormat.format(file.modifiedAt)}',
                      ),
                      trailing: FilledButton.tonal(
                        onPressed: _busyDecode
                            ? null
                            : () => _openLogFile(file),
                        child: const Text('Open'),
                      ),
                    ),
                  ),
              ],
            ),
          const SizedBox(height: 14),
          Container(
            width: double.infinity,
            constraints: const BoxConstraints(minHeight: 220),
            padding: const EdgeInsets.all(16),
            decoration: BoxDecoration(
              color: const Color(0xFF0F172A),
              borderRadius: BorderRadius.circular(22),
            ),
            child: _busyDecode
                ? const Center(child: CircularProgressIndicator())
                : SelectableText(
                    _decodedFileContent.isEmpty
                        ? 'Select a file to inspect decoded content.'
                        : _decodedFileContent,
                    style: _monoStyle(
                      fontSize: 12.5,
                      color: const Color(0xFFE2E8F0),
                      height: 1.55,
                    ),
                  ),
          ),
        ],
      ),
    );
  }

  Widget _buildMetricsCard() {
    return _panel(
      title: 'Metrics',
      subtitle:
          '通过 Prometheus exporter handle 抓当前 runtime 指标，既能看摘要，也能看原始 exposition。',
      child: Column(
        children: [
          Row(
            children: [
              FilledButton.tonalIcon(
                onPressed: !_busyMetrics ? _refreshMetrics : null,
                icon: _busyMetrics
                    ? const SizedBox.square(
                        dimension: 16,
                        child: CircularProgressIndicator(strokeWidth: 2),
                      )
                    : const Icon(Icons.analytics),
                label: const Text('Refresh Metrics'),
              ),
            ],
          ),
          const SizedBox(height: 16),
          Wrap(
            spacing: 10,
            runSpacing: 10,
            children: _metricTiles.isEmpty
                ? [_metricBadge('Status', 'No samples yet')]
                : _metricTiles
                      .map((item) => _metricBadge(item.title, item.value))
                      .toList(growable: false),
          ),
          const SizedBox(height: 14),
          Container(
            width: double.infinity,
            constraints: const BoxConstraints(minHeight: 220),
            padding: const EdgeInsets.all(16),
            decoration: BoxDecoration(
              color: const Color(0xFFFFFBF5),
              borderRadius: BorderRadius.circular(22),
              border: Border.all(color: const Color(0xFFE7E0D2)),
            ),
            child: SelectableText(
              _metricsSnapshot.isEmpty
                  ? 'Metrics snapshot will appear here after logger initialization.'
                  : _metricsSnapshot,
              style: _monoStyle(
                fontSize: 12.5,
                color: const Color(0xFF334155),
                height: 1.45,
              ),
            ),
          ),
        ],
      ),
    );
  }

  Widget _buildActivityCard() {
    return _panel(
      title: 'Activity Feed',
      subtitle: '记录当前 example 的关键动作，便于和 log / metrics 文件互相对照。',
      child: _activity.isEmpty
          ? _emptyState('Activity messages will appear here.')
          : Column(
              children: [
                for (final item in _activity)
                  Container(
                    width: double.infinity,
                    margin: const EdgeInsets.only(bottom: 10),
                    padding: const EdgeInsets.all(12),
                    decoration: BoxDecoration(
                      color: const Color(0xFFF8F5EF),
                      borderRadius: BorderRadius.circular(16),
                    ),
                    child: Text(
                      item,
                      style: _monoStyle(
                        fontSize: 12.5,
                        color: const Color(0xFF334155),
                      ),
                    ),
                  ),
              ],
            ),
    );
  }

  Widget _panel({
    required String title,
    required String subtitle,
    required Widget child,
  }) {
    return Container(
      width: double.infinity,
      margin: const EdgeInsets.only(bottom: 20),
      padding: const EdgeInsets.all(20),
      decoration: BoxDecoration(
        color: const Color(0xFFFFFBF5),
        borderRadius: BorderRadius.circular(28),
        boxShadow: const [
          BoxShadow(
            color: Color(0x14000000),
            blurRadius: 18,
            offset: Offset(0, 10),
          ),
        ],
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            title,
            style: Theme.of(
              context,
            ).textTheme.headlineSmall?.copyWith(fontWeight: FontWeight.w700),
          ),
          const SizedBox(height: 6),
          Text(
            subtitle,
            style: Theme.of(context).textTheme.bodyMedium?.copyWith(
              color: const Color(0xFF475569),
              height: 1.45,
            ),
          ),
          const SizedBox(height: 18),
          child,
        ],
      ),
    );
  }

  Widget _heroChip(String label, Color color) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 12, vertical: 8),
      decoration: BoxDecoration(
        color: color,
        borderRadius: BorderRadius.circular(999),
      ),
      child: Text(
        label,
        style: _monoStyle(
          color: Colors.white,
          fontSize: 12,
          fontWeight: FontWeight.w600,
        ),
      ),
    );
  }

  Widget _statusTile({
    required String title,
    required String value,
    required Color accent,
  }) {
    return Container(
      constraints: const BoxConstraints(minWidth: 180),
      padding: const EdgeInsets.all(14),
      decoration: BoxDecoration(
        color: Colors.white.withValues(alpha: 0.12),
        borderRadius: BorderRadius.circular(20),
        border: Border.all(color: Colors.white.withValues(alpha: 0.12)),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            title,
            style: _monoStyle(
              fontSize: 11.5,
              color: Colors.white.withValues(alpha: 0.7),
            ),
          ),
          const SizedBox(height: 8),
          Text(
            value,
            maxLines: 2,
            overflow: TextOverflow.ellipsis,
            style: Theme.of(context).textTheme.titleMedium?.copyWith(
              color: accent,
              fontWeight: FontWeight.w700,
            ),
          ),
        ],
      ),
    );
  }

  Widget _metricBadge(String label, String value) {
    return Container(
      padding: const EdgeInsets.symmetric(horizontal: 14, vertical: 10),
      decoration: BoxDecoration(
        color: const Color(0xFFF0E7D8),
        borderRadius: BorderRadius.circular(18),
      ),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Text(
            label,
            style: _monoStyle(fontSize: 11, color: const Color(0xFF64748B)),
          ),
          const SizedBox(height: 6),
          Text(
            value,
            style: Theme.of(
              context,
            ).textTheme.titleMedium?.copyWith(fontWeight: FontWeight.w700),
          ),
        ],
      ),
    );
  }

  Widget _textField({
    required TextEditingController controller,
    required String label,
    required String hint,
    int maxLines = 1,
  }) {
    return TextField(
      controller: controller,
      maxLines: maxLines,
      decoration: InputDecoration(
        labelText: label,
        hintText: hint,
        border: OutlineInputBorder(borderRadius: BorderRadius.circular(18)),
        filled: true,
        fillColor: const Color(0xFFFFFDF8),
      ),
    );
  }

  Widget _segmentedField<T>({
    required String label,
    required List<T> values,
    required T current,
    required String Function(T) labelOf,
    required ValueChanged<T> onChanged,
  }) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        Padding(
          padding: const EdgeInsets.only(bottom: 8),
          child: Text(label, style: Theme.of(context).textTheme.labelLarge),
        ),
        SegmentedButton<T>(
          segments: values
              .map(
                (value) =>
                    ButtonSegment<T>(value: value, label: Text(labelOf(value))),
              )
              .toList(growable: false),
          selected: <T>{current},
          onSelectionChanged: (selection) => onChanged(selection.first),
          showSelectedIcon: false,
        ),
      ],
    );
  }

  Widget _emptyState(String message) {
    return Container(
      width: double.infinity,
      padding: const EdgeInsets.all(20),
      decoration: BoxDecoration(
        color: const Color(0xFFF8F5EF),
        borderRadius: BorderRadius.circular(18),
      ),
      child: Text(
        message,
        style: Theme.of(
          context,
        ).textTheme.bodyMedium?.copyWith(color: const Color(0xFF64748B)),
      ),
    );
  }

  List<_MetricTileData> _extractMetrics(String raw) {
    const watched = <String, String>{
      'xlog_async_stage_sample_total': 'Async samples',
      'xlog_sync_stage_sample_total': 'Sync samples',
      'xlog_core_file_append_total': 'File appends',
      'xlog_core_engine_flush_total': 'Flushes',
      'xlog_async_queue_depth': 'Queue depth',
    };
    final found = <_MetricTileData>[];

    for (final line in raw.split('\n')) {
      if (line.isEmpty || line.startsWith('#')) {
        continue;
      }
      final parts = line.trim().split(RegExp(r'\s+'));
      if (parts.length < 2) {
        continue;
      }
      final metricName = parts.first;
      final title = watched[metricName];
      if (title == null) {
        continue;
      }
      found.add(_MetricTileData(title, parts.last));
    }

    return found;
  }

  String _formatCompact(double value) {
    return NumberFormat.compact(locale: 'en_US').format(value);
  }

  String _formatBytes(double bytes) {
    if (bytes < 1024) {
      return '${bytes.toStringAsFixed(0)} B';
    }
    if (bytes < 1024 * 1024) {
      return '${(bytes / 1024).toStringAsFixed(1)} KB';
    }
    return '${(bytes / (1024 * 1024)).toStringAsFixed(2)} MB';
  }

  String _formatBytesPerSecond(double bytesPerSecond) {
    return '${_formatBytes(bytesPerSecond)}/s';
  }
}

class _MetricTileData {
  const _MetricTileData(this.title, this.value);

  final String title;
  final String value;
}
