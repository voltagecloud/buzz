import 'package:flutter/material.dart';
import 'package:flutter_svg/flutter_svg.dart';

/// The independent Commons mark. Callers own any accessibility label.
class CommonsMark extends StatelessWidget {
  /// Creates a mark that follows the surrounding foreground color.
  const CommonsMark({required this.size, required this.color, super.key});

  /// The available width and height.
  final double size;

  /// The foreground color.
  final Color color;

  @override
  Widget build(BuildContext context) => SvgPicture.asset(
    'assets/images/commons-mark.svg',
    width: size,
    height: size,
    colorFilter: ColorFilter.mode(color, BlendMode.srcIn),
    excludeFromSemantics: true,
  );
}
