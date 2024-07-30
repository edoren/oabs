/** @type {import('tailwindcss').Config} */
const plugin = require("tailwindcss/plugin");

module.exports = {
    // darkMode: ['selector'],
    content: ["./index.html", "./src/**/*.rs"],
    theme: {
        extend: {},
    },
    plugins: [
        function ({ addVariant }) {
            addVariant("slider-thumb", [
                "&::-webkit-slider-thumb",
                "&::slider-thumb",
            ]);
        },
    ],
};
