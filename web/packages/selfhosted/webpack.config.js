import url from "url";
import json5 from "json5";
import CopyPlugin from "copy-webpack-plugin";
import TerserPlugin from "terser-webpack-plugin";

function transformPackage(content) {
    const pkg = json5.parse(content);

    // Note: The npm registry requires the version to monotonically increase.
    pkg.version = process.env.npm_package_version;

    return JSON.stringify(pkg);
}

export default function (_env, _argv) {
    const mode = process.env.NODE_ENV || "production";
    console.log(`Building ${mode}...`);

    return {
        mode,
        entry: "./js/ruffle.js",
        output: {
            path: url.fileURLToPath(new URL("dist", import.meta.url)),
            filename: "ruffle.js",
            publicPath: "",
            // Nomes fixos (sem [contenthash]) de proposito: o AQWLite embute
            // esses arquivos como recurso no .exe (resources/resource.rc) e
            // os extrai pelo nome exato em App.cpp. Se o hash mudasse a cada
            // build, toda mudanca no Ruffle quebraria o launcher em silencio
            // ate alguem atualizar os dois arquivos C++ na mao.
            chunkFilename: "core.ruffle.js",
            clean: true,
        },
        performance: {
            assetFilter: (assetFilename) =>
                !/\.(map|wasm)$/i.test(assetFilename),
        },
        module: {
            rules: [
                {
                    // Mesmo motivo do chunkFilename acima: forca o .wasm a
                    // sair sempre com o mesmo nome, em vez do default do
                    // webpack ([hash].wasm).
                    test: /\.wasm$/,
                    type: "asset/resource",
                    generator: {
                        filename: "core.ruffle.wasm",
                    },
                },
            ],
        },
        optimization: {
            minimizer: [
                new TerserPlugin({
                    terserOptions: {
                        output: {
                            ascii_only: true,
                        },
                    },
                }),
            ],
        },
        devtool: "source-map",
        plugins: [
            new CopyPlugin({
                patterns: [
                    {
                        from: "npm-package.json5",
                        to: "package.json",
                        transform: transformPackage,
                    },
                    { from: "LICENSE*" },
                    { from: "README.md" },
                ],
            }),
        ],
    };
}
