<?php
/**
 * Copyright © Cresset. All rights reserved.
 * See COPYING.txt for license details.
 */
declare(strict_types=1);

namespace Cresset\MagecommandLess\Adapter;

use Magento\Framework\App\State;
use Magento\Framework\Css\PreProcessor\File\Temporary;
use Magento\Framework\Phrase;
use Magento\Framework\View\Asset\ContentProcessorException;
use Magento\Framework\View\Asset\ContentProcessorInterface;
use Magento\Framework\View\Asset\File;
use Magento\Framework\View\Asset\Source;
use Psr\Log\LoggerInterface;
use Symfony\Component\Process\Process;

/**
 * LESS-to-CSS adapter backed by the `magecommand` Rust compiler.
 *
 * Drop-in replacement for {@see \Magento\Framework\Css\PreProcessor\Adapter\Less\Processor}
 * (wired via a di.xml preference): the preprocessor chain still materializes
 * every entry under var/view_preprocessed exactly as stock does — only the
 * final LESS compile is delegated to `magecommand static less --file`, which
 * mirrors `Less_Parser` semantics (including `compress` outside developer
 * mode) at a fraction of the runtime.
 */
class Processor implements ContentProcessorInterface
{
    /**
     * Environment variable overriding the configured binary path.
     */
    private const BIN_ENV = 'MAGECOMMAND_BIN';

    /**
     * Seconds one entry-point compile may take before we give up.
     */
    private const TIMEOUT_SECONDS = 300;

    /**
     * @var LoggerInterface
     */
    private LoggerInterface $logger;

    /**
     * @var State
     */
    private State $appState;

    /**
     * @var Source
     */
    private Source $assetSource;

    /**
     * @var Temporary
     */
    private Temporary $temporaryFile;

    /**
     * @var string
     */
    private string $magecommandBin;

    /**
     * Constructor
     *
     * @param LoggerInterface $logger
     * @param State $appState
     * @param Source $assetSource
     * @param Temporary $temporaryFile
     * @param string $magecommandBin Binary to run; a bare name resolves on PATH.
     *        The MAGECOMMAND_BIN environment variable overrides this argument.
     */
    public function __construct(
        LoggerInterface $logger,
        State $appState,
        Source $assetSource,
        Temporary $temporaryFile,
        string $magecommandBin = 'magecommand'
    ) {
        $this->logger = $logger;
        $this->appState = $appState;
        $this->assetSource = $assetSource;
        $this->temporaryFile = $temporaryFile;
        $envBin = getenv(self::BIN_ENV);
        $this->magecommandBin = $envBin !== false && $envBin !== '' ? $envBin : $magecommandBin;
    }

    /**
     * @inheritdoc
     */
    public function processContent(File $asset)
    {
        $path = $asset->getPath();
        try {
            $compress = $this->appState->getMode() !== State::MODE_DEVELOPER;

            $content = $this->assetSource->getContent($asset);

            if ($content === null || trim($content) === '') {
                throw new ContentProcessorException(
                    new Phrase('Compilation from source: LESS file is empty: ' . $path)
                );
            }

            $tmpFilePath = $this->temporaryFile->createFile($path, $content);

            $command = [$this->magecommandBin, 'static', 'less', '--file', $tmpFilePath, '--stdout'];
            if ($compress) {
                $command[] = '--compress';
            }

            $process = new Process($command);
            $process->setTimeout(self::TIMEOUT_SECONDS);
            $process->run();

            if (!$process->isSuccessful()) {
                $stderr = trim($process->getErrorOutput());
                if ($stderr === '') {
                    $stderr = 'magecommand exited with code ' . $process->getExitCode();
                }
                throw new ContentProcessorException(
                    new Phrase('Compilation from source: ' . $stderr)
                );
            }

            $content = $process->getOutput();

            $stderr = trim($process->getErrorOutput());
            if ($stderr !== '') {
                $this->logger->warning('magecommand static less: ' . $path . ': ' . $stderr);
            }

            if (trim($content) === '') {
                throw new ContentProcessorException(
                    new Phrase('Compilation from source: LESS file is empty: ' . $path)
                );
            }

            return $content;
        } catch (ContentProcessorException $e) {
            throw $e;
        } catch (\Exception $e) {
            throw new ContentProcessorException(new Phrase($e->getMessage()));
        }
    }
}
